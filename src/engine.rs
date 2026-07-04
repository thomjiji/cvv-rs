use crate::directio::{DestWriter, BUFFER_SIZE};
use crate::progress::Progress;
use filetime::{set_file_times, FileTime};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Instant;
use xxhash_rust::xxh3::Xxh3;

// Chunks in flight per destination; bounds memory (~128MB) and decouples mixed-speed
// targets so a slow destination doesn't stall the reader or faster destinations.
const QUEUE_DEPTH: usize = 16;

// Fills buf completely from f, looping past short reads until full or EOF; returns the
// number of bytes read. This keeps every chunk exactly BUFFER_SIZE except the last,
// which is what makes O_DIRECT tail handling in DestWriter sound.
fn read_full(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

#[derive(Debug)]
pub struct FileEntry {
    pub relative_path: PathBuf,
    pub absolute_path: PathBuf,
    pub size: u64,
}

impl FileEntry {
    // Returns (files, dirs); dirs holds every directory's relative path so empty dirs
    // can be recreated at the destination even though they carry no FileEntry.
    pub fn discover(source: &Path) -> Result<(Vec<FileEntry>, Vec<PathBuf>), io::Error> {
        if source.is_file() {
            let meta = fs::metadata(source)?;
            let name = source
                .file_name()
                .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no file name"))?;
            return Ok((
                vec![FileEntry {
                    relative_path: PathBuf::from(name),
                    absolute_path: source.to_path_buf(),
                    size: meta.len(),
                }],
                Vec::new(),
            ));
        }

        let mut entries = Vec::new();
        let mut dirs = Vec::new();
        collect_files(source, source, &mut entries, &mut dirs)?;
        entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        dirs.sort();
        Ok((entries, dirs))
    }
}

fn collect_files(
    root: &Path,
    dir: &Path,
    entries: &mut Vec<FileEntry>,
    dirs: &mut Vec<PathBuf>,
) -> Result<(), io::Error> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        // file_type() does not follow symlinks (unlike path.is_dir()/is_file()), so a
        // symlinked directory can't send us into infinite recursion.
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            eprintln!("Warning: skipping symlink: {}", path.display());
            continue;
        }
        if file_type.is_dir() {
            let relative = path
                .strip_prefix(root)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            dirs.push(relative.to_path_buf());
            collect_files(root, &path, entries, dirs)?;
        } else if file_type.is_file() {
            let meta = fs::metadata(&path)?;
            let relative = path
                .strip_prefix(root)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            entries.push(FileEntry {
                relative_path: relative.to_path_buf(),
                absolute_path: path,
                size: meta.len(),
            });
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct CopyResult {
    pub relative_path: PathBuf,
    pub source_path: PathBuf,
    pub size: u64,
    pub inflight_hash: String,
    pub success: bool,
    pub skipped: bool,
    #[allow(dead_code)]
    pub error: Option<String>,
}

fn should_skip(dest_path: &Path, source_size: u64) -> bool {
    if let Ok(meta) = fs::metadata(dest_path) {
        meta.len() == source_size
    } else {
        false
    }
}

fn cleanup_tmp(dest_path: &Path) {
    let tmp_path = tmp_path_for(dest_path);
    if tmp_path.exists() {
        let _ = fs::remove_file(&tmp_path);
    }
}

fn tmp_path_for(dest_path: &Path) -> PathBuf {
    let mut name = dest_path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    dest_path.with_file_name(name)
}

fn format_size(n: u64) -> String {
    if n >= 1_073_741_824 {
        format!("{:.1} GB", n as f64 / 1_073_741_824.0)
    } else if n >= 1_048_576 {
        format!("{:.1} MB", n as f64 / 1_048_576.0)
    } else if n >= 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

fn copy_single_file(
    source: &Path,
    dest_paths: &[PathBuf],
    aborted: &Arc<AtomicBool>,
    mut on_progress: impl FnMut(u64),
) -> Result<(u64, String), io::Error> {
    let mut src = File::open(source)?;
    let src_meta = src.metadata()?;
    let mtime = FileTime::from_last_modification_time(&src_meta);
    let atime = FileTime::from_last_access_time(&src_meta);

    let file_size = src_meta.len();
    let mut targets: Vec<(PathBuf, PathBuf, Option<DestWriter>)> = Vec::new();
    for dest in dest_paths {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = tmp_path_for(dest);
        let w = DestWriter::create(&tmp, file_size)?;
        targets.push((dest.clone(), tmp, Some(w)));
    }

    // Reader thread streams chunks to one writer thread per destination through
    // bounded channels, so the source read never stalls on a destination write.
    let copy_result = std::thread::scope(|s| {
        let mut senders = Vec::with_capacity(targets.len());
        let mut handles = Vec::with_capacity(targets.len());
        for (_, _, w) in &mut targets {
            let mut w = w.take().unwrap();
            let (tx, rx) = mpsc::sync_channel::<Arc<Vec<u8>>>(QUEUE_DEPTH);
            senders.push(tx);
            handles.push(s.spawn(move || -> io::Result<()> {
                for chunk in rx {
                    w.write_chunk(&chunk)?;
                }
                w.finish()
            }));
        }

        let mut hasher = Xxh3::new();
        let mut total_bytes: u64 = 0;
        let mut read_result: io::Result<()> = Ok(());

        loop {
            if aborted.load(Ordering::Relaxed) {
                read_result = Err(io::Error::new(io::ErrorKind::Interrupted, "aborted"));
                break;
            }
            let mut buf = vec![0u8; BUFFER_SIZE];
            match read_full(&mut src, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    buf.truncate(n);
                    hasher.update(&buf);
                    let chunk = Arc::new(buf);
                    // A failed send means that writer died; its error surfaces on join.
                    if senders.iter().any(|tx| tx.send(chunk.clone()).is_err()) {
                        break;
                    }
                    total_bytes += n as u64;
                    on_progress(total_bytes);
                }
                Err(e) => {
                    read_result = Err(e);
                    break;
                }
            }
        }

        drop(senders);
        let mut write_result: io::Result<()> = Ok(());
        for h in handles {
            let r = h.join().unwrap();
            if write_result.is_ok() {
                write_result = r;
            }
        }
        read_result
            .and(write_result)
            .map(|_| (total_bytes, format!("{:016X}", hasher.digest())))
    });

    match copy_result {
        Ok((total_bytes, hash)) => {
            for (dest, tmp, _) in &targets {
                fs::rename(tmp, dest)?;
                set_file_times(dest, atime, mtime)?;
            }
            Ok((total_bytes, hash))
        }
        Err(e) => {
            for (_, tmp, _) in &targets {
                let _ = fs::remove_file(tmp);
            }
            Err(e)
        }
    }
}

pub fn copy_all(
    _source_root: &Path,
    files: &[FileEntry],
    destinations: &[PathBuf],
    aborted: &Arc<AtomicBool>,
) -> Vec<CopyResult> {
    let total_files = files.len();
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let mut bytes_done: u64 = 0;
    let mut copied_bytes: u64 = 0;
    let mut results = Vec::with_capacity(total_files);
    let start = Instant::now();
    let progress = Progress::new(total_bytes);

    for (i, file) in files.iter().enumerate() {
        if aborted.load(Ordering::Relaxed) {
            break;
        }

        let dest_paths: Vec<PathBuf> = destinations.iter().map(|d| d.join(&file.relative_path)).collect();

        let dests_to_copy: Vec<PathBuf> = dest_paths
            .iter()
            .filter(|d| {
                cleanup_tmp(d);
                !should_skip(d, file.size)
            })
            .cloned()
            .collect();

        if dests_to_copy.is_empty() {
            progress.println(format!(
                "[{}/{}] Skipped {} (already exists)",
                i + 1,
                total_files,
                file.relative_path.display()
            ));
            bytes_done += file.size;
            progress.advance_overall(file.size);
            results.push(CopyResult {
                relative_path: file.relative_path.clone(),
                source_path: file.absolute_path.clone(),
                size: file.size,
                inflight_hash: String::new(),
                success: true,
                skipped: true,
                error: None,
            });
            continue;
        }

        let file_start = Instant::now();
        let file_size = file.size;
        let short_name = file
            .relative_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let idx = i + 1;
        let mut last_update = Instant::now();
        let bytes_done_before = bytes_done;
        progress.file_start(&format!("{idx}/{total_files}"), &short_name, file_size);

        let file_progress = |file_bytes: u64| {
            let now = Instant::now();
            if now.duration_since(last_update).as_millis() < 100 {
                return;
            }
            last_update = now;
            progress.update(file_bytes, bytes_done_before + file_bytes);
        };

        match copy_single_file(&file.absolute_path, &dests_to_copy, aborted, file_progress) {
            Ok((bytes, hash)) => {
                bytes_done += bytes;
                copied_bytes += bytes;
                progress.update(bytes, bytes_done);
                let elapsed = file_start.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 {
                    bytes as f64 / 1_048_576.0 / elapsed
                } else {
                    0.0
                };
                progress.println(format!(
                    "[{}/{}] Copied {}  {}  {}  {:.1} MB/s",
                    idx,
                    total_files,
                    file.relative_path.display(),
                    hash,
                    format_size(bytes),
                    speed,
                ));
                results.push(CopyResult {
                    relative_path: file.relative_path.clone(),
                    source_path: file.absolute_path.clone(),
                    size: bytes,
                    inflight_hash: hash,
                    success: true,
                    skipped: false,
                    error: None,
                });
            }
            Err(e) => {
                if e.kind() == io::ErrorKind::Interrupted {
                    break;
                }
                progress.suspend(|| {
                    eprintln!(
                        "[{}/{}] FAILED {}  {}",
                        idx,
                        total_files,
                        file.relative_path.display(),
                        e
                    );
                });
                results.push(CopyResult {
                    relative_path: file.relative_path.clone(),
                    source_path: file.absolute_path.clone(),
                    size: file.size,
                    inflight_hash: String::new(),
                    success: false,
                    skipped: false,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    progress.finish();

    let elapsed = start.elapsed().as_secs_f64();
    let copied = results.iter().filter(|r| r.success && !r.skipped).count();
    let skipped = results.iter().filter(|r| r.skipped).count();
    if copied_bytes == 0 {
        println!("\nCopy complete: {} copied, {} skipped", copied, skipped);
    } else {
        let avg_speed = if elapsed > 0.0 {
            copied_bytes as f64 / 1_048_576.0 / elapsed
        } else {
            0.0
        };
        println!(
            "\nCopy complete: {} copied, {} skipped, {:.1} MB/s avg",
            copied, skipped, avg_speed
        );
    }

    results
}
