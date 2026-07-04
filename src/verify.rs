use crate::directio::{open_no_cache, AlignedBuffer, BUFFER_SIZE};
use crate::engine::CopyResult;
use crate::progress::DeviceProgress;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use xxhash_rust::xxh3::Xxh3;

#[derive(Debug, Clone, Copy)]
pub enum VerifyMode {
    Source,
    Target,
    Transfer,
}

// Hashes one file using a reused aligned buffer, checking the abort flag every chunk
// and accumulating progress into the device's byte counter.
fn hash_one(
    path: &Path,
    buffer: &mut AlignedBuffer,
    aborted: &AtomicBool,
    bytes: &AtomicU64,
) -> Result<String, io::Error> {
    let mut f = open_no_cache(path)?;
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
        bytes.fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok(format!("{:016X}", hasher.digest()))
}

// One worker per device: streams its whole file list in seq order at its own pace,
// never blocking on other devices. Allocates a single aligned buffer for all its files.
fn verify_worker(
    device: usize,
    files: Vec<(usize, PathBuf)>,
    aborted: &AtomicBool,
    bytes: &AtomicU64,
    tx: mpsc::Sender<(usize, usize, Result<String, io::Error>)>,
) {
    let mut buffer = AlignedBuffer::new(BUFFER_SIZE);
    for (seq, path) in files {
        if aborted.load(Ordering::Relaxed) {
            let _ = tx.send((
                seq,
                device,
                Err(io::Error::new(io::ErrorKind::Interrupted, "aborted")),
            ));
            break;
        }
        let result = hash_one(&path, &mut buffer, aborted, bytes);
        if tx.send((seq, device, result)).is_err() {
            break; // coordinator gone
        }
    }
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
//
// Each device runs its own pipeline worker that streams the entire file list at its own
// pace; the coordinator compares hashes as files complete, so a fast disk never idles
// waiting on a slow one.
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

    let with_source = matches!(mode, VerifyMode::Source);
    let dest_offset = with_source as usize;
    let num_devices = destinations.len() + dest_offset;

    // Devices in fixed order: [source] (source mode) then destinations in CLI order.
    let mut device_files: Vec<Vec<(usize, PathBuf)>> =
        (0..num_devices).map(|_| Vec::with_capacity(total)).collect();
    let mut device_totals: Vec<u64> = vec![0; num_devices];
    let mut labels: Vec<String> = Vec::with_capacity(num_devices);
    if with_source {
        labels.push("src".to_string());
    }
    for d in destinations {
        labels.push(d.display().to_string());
    }
    for (seq, &ri) in indices.iter().enumerate() {
        let size = results[ri].size;
        if with_source {
            device_files[0].push((seq, results[ri].source_path.clone()));
            device_totals[0] += size;
        }
        for (k, d) in destinations.iter().enumerate() {
            let dev = dest_offset + k;
            device_files[dev].push((seq, d.join(&results[ri].relative_path)));
            device_totals[dev] += size;
        }
    }
    let overall_total: u64 = device_totals.iter().sum();

    let device_labels: Vec<(String, u64)> = labels.into_iter().zip(device_totals).collect();
    let progress = DeviceProgress::new(&device_labels, overall_total);
    let counters: Vec<Arc<AtomicU64>> =
        (0..num_devices).map(|_| Arc::new(AtomicU64::new(0))).collect();

    let start = Instant::now();
    let mut all_ok = true;
    let mut ok_count = 0;
    let mut aborted_flag = false;

    // pending[seq][device] = Some(result) once that device has hashed file `seq`.
    let mut pending: Vec<Vec<Option<Result<String, io::Error>>>> = (0..total)
        .map(|_| (0..num_devices).map(|_| None).collect())
        .collect();
    let mut next_seq = 0usize;

    std::thread::scope(|s| {
        let (tx, rx) = mpsc::channel::<(usize, usize, Result<String, io::Error>)>();
        for (device, files) in device_files.into_iter().enumerate() {
            let tx = tx.clone();
            let counter = counters[device].clone();
            let aborted = aborted.clone();
            s.spawn(move || verify_worker(device, files, &aborted, &counter, tx));
        }
        drop(tx); // channel closes once all workers finish

        loop {
            let disconnected = match rx.recv_timeout(Duration::from_millis(100)) {
                Ok((seq, device, result)) => {
                    pending[seq][device] = Some(result);
                    false
                }
                Err(mpsc::RecvTimeoutError::Timeout) => false,
                Err(mpsc::RecvTimeoutError::Disconnected) => true,
            };

            for i in 0..num_devices {
                progress.set_device(i, counters[i].load(Ordering::Relaxed));
            }
            progress.set_overall(counters.iter().map(|c| c.load(Ordering::Relaxed)).sum());

            // Files complete in seq order because every worker walks the same order.
            while next_seq < total && pending[next_seq].iter().all(|x| x.is_some()) {
                let ri = indices[next_seq];
                let idx = next_seq + 1;
                let file_name = results[ri].relative_path.display().to_string();
                let expected = results[ri].inflight_hash.clone();
                let slots: Vec<Result<String, io::Error>> = std::mem::take(&mut pending[next_seq])
                    .into_iter()
                    .map(|o| o.unwrap())
                    .collect();

                // Any Interrupted result means a global abort is in progress.
                if slots
                    .iter()
                    .any(|r| matches!(r, Err(e) if e.kind() == io::ErrorKind::Interrupted))
                {
                    aborted_flag = true;
                    break;
                }

                let mut file_ok = true;
                let mut ref_from_target: Option<usize> = None;
                let reference: Option<String> = if with_source {
                    match &slots[0] {
                        Ok(h) => {
                            let h = h.clone();
                            if !expected.is_empty() && h != expected {
                                progress.suspend(|| {
                                    eprintln!(
                                        "[{}/{}] FAIL {} (source changed: {} vs inflight {})",
                                        idx, total, file_name, h, expected
                                    );
                                });
                                file_ok = false;
                            }
                            Some(h)
                        }
                        Err(e) => {
                            progress.suspend(|| {
                                eprintln!(
                                    "[{}/{}] FAIL {} (source read error: {})",
                                    idx, total, file_name, e
                                );
                            });
                            file_ok = false;
                            None
                        }
                    }
                } else if !expected.is_empty() {
                    Some(expected.clone())
                } else {
                    // Skipped file in target mode: reference is the lowest-index target
                    // that read OK; a mismatch then only proves the targets disagree.
                    slots.iter().position(|r| r.is_ok()).map(|p| {
                        ref_from_target = Some(p);
                        slots[p].as_ref().unwrap().clone()
                    })
                };

                for j in 0..destinations.len() {
                    match (&slots[dest_offset + j], &reference) {
                        (Ok(hash), Some(r)) if hash != r => {
                            progress.suspend(|| {
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
                            });
                            file_ok = false;
                        }
                        (Ok(_), _) => {}
                        (Err(e), _) => {
                            progress.suspend(|| {
                                eprintln!(
                                    "[{}/{}] FAIL {} (dest read error: {})",
                                    idx, total, file_name, e
                                );
                            });
                            file_ok = false;
                        }
                    }
                }

                if let Some(r) = reference {
                    if results[ri].inflight_hash.is_empty() && file_ok {
                        results[ri].inflight_hash = r;
                    }
                }
                if file_ok {
                    ok_count += 1;
                    let hash = results[ri].inflight_hash.clone();
                    progress.println(format!("[{}/{}] Verified {}  {}", idx, total, file_name, hash));
                } else {
                    all_ok = false;
                }
                next_seq += 1;
            }

            if aborted_flag || next_seq == total {
                break;
            }
            if disconnected {
                // Workers all exited but a file is still incomplete: a worker died early
                // (unreachable in practice). Fail the file so integrity isn't overstated.
                if next_seq < total {
                    let ri = indices[next_seq];
                    all_ok = false;
                    progress.suspend(|| {
                        eprintln!(
                            "[{}/{}] FAIL {} (worker exited before completing verification)",
                            next_seq + 1,
                            total,
                            results[ri].relative_path.display()
                        );
                    });
                }
                break;
            }
        }
    });

    progress.finish();

    if aborted_flag {
        return false;
    }

    let bytes_done: u64 = counters.iter().map(|c| c.load(Ordering::Relaxed)).sum();
    let elapsed = start.elapsed().as_secs_f64();
    let avg_speed = if elapsed > 0.0 {
        bytes_done as f64 / 1_048_576.0 / elapsed
    } else {
        0.0
    };
    println!("\nVerification complete: {ok_count}/{total} OK, {avg_speed:.1} MB/s avg");

    all_ok
}
