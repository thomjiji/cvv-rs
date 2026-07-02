use crate::engine::CopyResult;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use xxhash_rust::xxh3::Xxh3;

const BUFFER_SIZE: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub enum VerifyMode {
    Source,
    Target,
    Transfer,
}

struct AlignedBuffer {
    ptr: *mut u8,
    layout: std::alloc::Layout,
    size: usize,
}

impl AlignedBuffer {
    fn new(size: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(size, 4096).unwrap();
        let ptr = unsafe { std::alloc::alloc(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Self { ptr, layout, size }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.size) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr, self.layout) }
    }
}

// SAFETY: AlignedBuffer is just an owned heap allocation, safe to send across threads.
unsafe impl Send for AlignedBuffer {}

#[cfg(windows)]
fn open_no_cache(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x20000000) // FILE_FLAG_NO_BUFFERING
        .open(path)
}

#[cfg(target_os = "macos")]
fn open_no_cache(path: &Path) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    let f = File::open(path)?;
    if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_NOCACHE, 1) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(f)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_no_cache(path: &Path) -> io::Result<File> {
    // O_DIRECT needs 4096-aligned buffers (AlignedBuffer) and chunk-multiple reads.
    // Falls back to buffered IO on filesystems that reject O_DIRECT (e.g. tmpfs).
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .or_else(|_| File::open(path))
}

fn hash_file_thread(
    path: &Path,
    aborted: &AtomicBool,
    shared_bytes: &AtomicU64,
) -> Result<String, io::Error> {
    let mut f = open_no_cache(path)?;
    let mut buffer = AlignedBuffer::new(BUFFER_SIZE);
    let mut hasher = Xxh3::new();

    loop {
        if aborted.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "aborted"));
        }
        let n = f.read(buffer.as_mut_slice())?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer.as_mut_slice()[..n]);
        shared_bytes.fetch_add(n as u64, Ordering::Relaxed);
    }

    Ok(format!("{:016X}", hasher.digest()))
}

fn hash_paths_parallel(
    paths: &[PathBuf],
    aborted: &Arc<AtomicBool>,
    idx: usize,
    total_files: usize,
    file_name: &str,
    phase: &str,
    hash_total_bytes: u64,
    bytes_done: u64,
    overall_total_bytes: u64,
) -> Vec<Result<String, io::Error>> {
    let shared_bytes = AtomicU64::new(0);
    let done = AtomicBool::new(false);
    let file_start = Instant::now();

    std::thread::scope(|s| {
        let handles: Vec<_> = paths
            .iter()
            .map(|path| s.spawn(|| hash_file_thread(path, aborted, &shared_bytes)))
            .collect();

        s.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                if done.load(Ordering::Relaxed) {
                    break;
                }
                let hashed = shared_bytes.load(Ordering::Relaxed);
                let elapsed = file_start.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 {
                    hashed as f64 / 1_048_576.0 / elapsed
                } else {
                    0.0
                };
                let file_pct = if hash_total_bytes > 0 {
                    hashed as f64 / hash_total_bytes as f64 * 100.0
                } else {
                    100.0
                };
                let overall_pct = if overall_total_bytes > 0 {
                    (bytes_done + hashed.min(hash_total_bytes)) as f64
                        / overall_total_bytes as f64
                        * 100.0
                } else {
                    100.0
                };
                print!(
                    "\r[{}/{}] {} {}  {:.1}%  {:.1} MB/s  Overall: {:.1}%    ",
                    idx, total_files, phase, file_name, file_pct, speed, overall_pct,
                );
                let _ = io::stdout().flush();
            }
        });

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        done.store(true, Ordering::Relaxed);
        results
    })
}

fn verify_transfer(results: &[CopyResult], destinations: &[PathBuf]) -> bool {
    let mut all_ok = true;
    let mut checked = 0;
    for r in results.iter().filter(|r| r.success) {
        for d in destinations {
            let path = d.join(&r.relative_path);
            match std::fs::metadata(&path) {
                Ok(m) if m.len() == r.size => {}
                Ok(m) => {
                    eprintln!(
                        "FAIL {} ({} bytes, expected {})",
                        path.display(),
                        m.len(),
                        r.size
                    );
                    all_ok = false;
                }
                Err(e) => {
                    eprintln!("FAIL {} ({})", path.display(), e);
                    all_ok = false;
                }
            }
            checked += 1;
        }
    }
    println!("Transfer check: {checked} file(s) size-compared");
    all_ok
}

// Verifies every successful result, including collision-skipped files: those have no
// in-flight hash, so the reference hash comes from the source (source mode) or the
// first target (target mode) and is written back so the hashfile stays complete.
pub fn verify_all(
    results: &mut [CopyResult],
    destinations: &[PathBuf],
    mode: VerifyMode,
    aborted: &Arc<AtomicBool>,
) -> bool {
    if matches!(mode, VerifyMode::Transfer) {
        return verify_transfer(results, destinations);
    }

    let indices: Vec<usize> = (0..results.len()).filter(|&i| results[i].success).collect();
    let total = indices.len();
    if total == 0 {
        println!("Nothing to verify.");
        return true;
    }

    let num_dests = destinations.len() as u64;
    let with_source = matches!(mode, VerifyMode::Source);
    let per_file_factor = num_dests + with_source as u64;
    let overall_total_bytes: u64 = indices.iter().map(|&i| results[i].size * per_file_factor).sum();

    let mut bytes_done: u64 = 0;
    let mut all_ok = true;
    let mut ok_count = 0;
    let start = Instant::now();

    for (i, &ri) in indices.iter().enumerate() {
        if aborted.load(Ordering::Relaxed) {
            return false;
        }

        let result = &results[ri];
        let idx = i + 1;
        let file_name = result.relative_path.display().to_string();
        let short_name = result
            .relative_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let expected = result.inflight_hash.clone();
        let hash_total = result.size * per_file_factor;
        let file_start = Instant::now();

        let mut paths: Vec<PathBuf> = Vec::with_capacity(per_file_factor as usize);
        if with_source {
            paths.push(result.source_path.clone());
        }
        paths.extend(destinations.iter().map(|d| d.join(&result.relative_path)));

        let mut hashes = hash_paths_parallel(
            &paths,
            aborted,
            idx,
            total,
            &short_name,
            "Verifying",
            hash_total,
            bytes_done,
            overall_total_bytes,
        );
        bytes_done += hash_total;

        if hashes
            .iter()
            .any(|h| matches!(h, Err(e) if e.kind() == io::ErrorKind::Interrupted))
        {
            return false;
        }

        // Reference hash: source hash in source mode, else in-flight hash,
        // else (skipped file in target mode) the first readable target's hash —
        // in that case a mismatch only proves the targets disagree, not which is bad.
        let mut file_ok = true;
        let mut ref_from_target: Option<usize> = None;
        let reference = if with_source {
            match hashes.remove(0) {
                Ok(h) => {
                    if !expected.is_empty() && h != expected {
                        print!("\r");
                        eprintln!(
                            "[{}/{}] FAIL {} (source changed: {} vs inflight {})",
                            idx, total, file_name, h, expected
                        );
                        file_ok = false;
                    }
                    Some(h)
                }
                Err(e) => {
                    print!("\r");
                    eprintln!("[{}/{}] FAIL {} (source read error: {})", idx, total, file_name, e);
                    file_ok = false;
                    None
                }
            }
        } else if !expected.is_empty() {
            Some(expected)
        } else {
            hashes.iter().position(|h| h.is_ok()).map(|p| {
                ref_from_target = Some(p);
                hashes[p].as_ref().unwrap().clone()
            })
        };

        for (j, hash_result) in hashes.iter().enumerate() {
            match (hash_result, &reference) {
                (Ok(hash), Some(r)) if hash != r => {
                    print!("\r");
                    if let Some(rj) = ref_from_target {
                        eprintln!(
                            "[{}/{}] FAIL {} (targets disagree: {} = {} vs {} = {})",
                            idx,
                            total,
                            file_name,
                            destinations[j].display(),
                            hash,
                            destinations[rj].display(),
                            r
                        );
                    } else {
                        eprintln!(
                            "[{}/{}] FAIL {} -> {} (hash mismatch: {} vs {})",
                            idx,
                            total,
                            file_name,
                            destinations[j].display(),
                            hash,
                            r
                        );
                    }
                    file_ok = false;
                }
                (Ok(_), _) => {}
                (Err(e), _) => {
                    print!("\r");
                    eprintln!("[{}/{}] FAIL {} (dest read error: {})", idx, total, file_name, e);
                    file_ok = false;
                }
            }
        }

        if let Some(r) = reference {
            if results[ri].inflight_hash.is_empty() && file_ok {
                results[ri].inflight_hash = r;
            }
        }
        if !file_ok {
            all_ok = false;
            continue;
        }
        ok_count += 1;

        let elapsed = file_start.elapsed().as_secs_f64();
        let speed = if elapsed > 0.0 {
            hash_total as f64 / 1_048_576.0 / elapsed
        } else {
            0.0
        };
        let overall_pct = if overall_total_bytes > 0 {
            bytes_done as f64 / overall_total_bytes as f64 * 100.0
        } else {
            100.0
        };
        print!(
            "\r[{}/{}] Verified {}  {}  {:.1} MB/s  Overall: {:.1}%    \n",
            idx, total, file_name, results[ri].inflight_hash, speed, overall_pct,
        );
        let _ = io::stdout().flush();
    }

    let elapsed = start.elapsed().as_secs_f64();
    let avg_speed = if elapsed > 0.0 {
        bytes_done as f64 / 1_048_576.0 / elapsed
    } else {
        0.0
    };
    println!(
        "\nVerification complete: {ok_count}/{total} OK, {avg_speed:.1} MB/s avg"
    );

    all_ok
}
