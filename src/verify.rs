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

#[cfg(not(windows))]
fn open_no_cache(path: &Path) -> io::Result<File> {
    // TODO: F_NOCACHE on macOS, O_DIRECT on Linux
    File::open(path)
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

pub fn verify_all(
    _source_root: &Path,
    results: &[CopyResult],
    destinations: &[PathBuf],
    mode: VerifyMode,
    aborted: &Arc<AtomicBool>,
) -> bool {
    let to_verify: Vec<&CopyResult> = results.iter().filter(|r| r.success && !r.skipped).collect();
    let total = to_verify.len();

    if total == 0 {
        println!("Nothing to verify (all files were skipped).");
        return true;
    }

    let num_dests = destinations.len() as u64;
    let overall_total_bytes: u64 = to_verify
        .iter()
        .map(|r| match mode {
            VerifyMode::Source => r.size * (1 + num_dests),
            VerifyMode::Target => r.size * num_dests,
            VerifyMode::Transfer => 0,
        })
        .sum();

    let mut bytes_done: u64 = 0;
    let mut all_ok = true;
    let start = Instant::now();

    for (i, result) in to_verify.iter().enumerate() {
        if aborted.load(Ordering::Relaxed) {
            return false;
        }

        let idx = i + 1;
        let file_name = result.relative_path.display().to_string();
        let short_name = result
            .relative_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let expected = &result.inflight_hash;
        let file_start = Instant::now();

        match mode {
            VerifyMode::Target => {
                let paths: Vec<PathBuf> = destinations
                    .iter()
                    .map(|d| d.join(&result.relative_path))
                    .collect();
                let hash_total = result.size * num_dests;

                let hashes = hash_paths_parallel(
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

                for (j, hash_result) in hashes.into_iter().enumerate() {
                    match hash_result {
                        Ok(hash) => {
                            if hash != *expected {
                                print!("\r");
                                eprintln!(
                                    "[{}/{}] FAIL {} -> {} (hash mismatch: {} vs {})",
                                    idx,
                                    total,
                                    file_name,
                                    destinations[j].display(),
                                    hash,
                                    expected
                                );
                                all_ok = false;
                            }
                        }
                        Err(e) => {
                            if e.kind() == io::ErrorKind::Interrupted {
                                return false;
                            }
                            print!("\r");
                            eprintln!("[{}/{}] FAIL {} ({})", idx, total, file_name, e);
                            all_ok = false;
                        }
                    }
                }
                bytes_done += hash_total;
            }
            VerifyMode::Source => {
                let mut paths: Vec<PathBuf> = vec![result.source_path.clone()];
                paths.extend(destinations.iter().map(|d| d.join(&result.relative_path)));
                let hash_total = result.size * (1 + num_dests);

                let hashes = hash_paths_parallel(
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

                let source_hash = match &hashes[0] {
                    Ok(h) => h.clone(),
                    Err(e) => {
                        if e.kind() == io::ErrorKind::Interrupted {
                            return false;
                        }
                        print!("\r");
                        eprintln!(
                            "[{}/{}] FAIL {} (source read error: {})",
                            idx, total, file_name, e
                        );
                        all_ok = false;
                        bytes_done += hash_total;
                        continue;
                    }
                };

                if source_hash != *expected {
                    print!("\r");
                    eprintln!(
                        "[{}/{}] FAIL {} (source changed: {} vs inflight {})",
                        idx, total, file_name, source_hash, expected
                    );
                    all_ok = false;
                }

                for (j, hash_result) in hashes[1..].iter().enumerate() {
                    match hash_result {
                        Ok(hash) => {
                            if *hash != source_hash {
                                print!("\r");
                                eprintln!(
                                    "[{}/{}] FAIL {} -> {} (hash mismatch: {} vs source {})",
                                    idx,
                                    total,
                                    file_name,
                                    destinations[j].display(),
                                    hash,
                                    source_hash
                                );
                                all_ok = false;
                            }
                        }
                        Err(e) => {
                            if e.kind() == io::ErrorKind::Interrupted {
                                return false;
                            }
                            print!("\r");
                            eprintln!(
                                "[{}/{}] FAIL {} (dest read error: {})",
                                idx, total, file_name, e
                            );
                            all_ok = false;
                        }
                    }
                }
                bytes_done += hash_total;
            }
            VerifyMode::Transfer => unreachable!(),
        }

        let elapsed = file_start.elapsed().as_secs_f64();
        let file_hash_bytes = match mode {
            VerifyMode::Source => result.size * (1 + num_dests),
            VerifyMode::Target => result.size * num_dests,
            VerifyMode::Transfer => unreachable!(),
        };
        let speed = if elapsed > 0.0 {
            file_hash_bytes as f64 / 1_048_576.0 / elapsed
        } else {
            0.0
        };
        let overall_pct = if overall_total_bytes > 0 {
            bytes_done as f64 / overall_total_bytes as f64 * 100.0
        } else {
            100.0
        };
        print!(
            "\r[{}/{}] Verified {}  {:.1} MB/s  Overall: {:.1}%    \n",
            idx, total, file_name, speed, overall_pct,
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
        "\nVerification complete: {}/{} OK, {:.1} MB/s avg",
        if all_ok { total } else { 0 },
        total,
        avg_speed
    );

    all_ok
}
