use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::sync::OnceLock;

pub const BUFFER_SIZE: usize = 8 * 1024 * 1024;

// Files at/above this size use the cache-bypass write path; smaller files stay buffered
// where page-cache overhead is negligible and write-through would only add latency.
pub const DIRECT_WRITE_THRESHOLD: u64 = 16 * 1024 * 1024;

pub struct AlignedBuffer {
    ptr: *mut u8,
    layout: std::alloc::Layout,
    size: usize,
}

impl AlignedBuffer {
    pub fn new(size: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(size, 4096).unwrap();
        let ptr = unsafe { std::alloc::alloc(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Self { ptr, layout, size }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
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
pub fn open_no_cache(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x20000000) // FILE_FLAG_NO_BUFFERING
        .open(path)
}

#[cfg(target_os = "macos")]
pub fn open_no_cache(path: &Path) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    let f = File::open(path)?;
    if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_NOCACHE, 1) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(f)
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn open_no_cache(path: &Path) -> io::Result<File> {
    // O_DIRECT needs 4096-aligned buffers (AlignedBuffer) and chunk-multiple reads.
    // Falls back to buffered IO on filesystems that reject O_DIRECT (e.g. tmpfs).
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .or_else(|_| File::open(path))
}

fn buffered_writes_forced() -> bool {
    static FORCED: OnceLock<bool> = OnceLock::new();
    *FORCED.get_or_init(|| std::env::var_os("CVV_BUFFERED_WRITES").is_some())
}

// Destination file that bypasses the OS page cache for large files so lazy-writer
// flushing doesn't cause throughput ripple. On Linux this is O_DIRECT (aligned writes
// with an unaligned tail flushed buffered); on Windows FILE_FLAG_WRITE_THROUGH; on
// macOS F_NOCACHE. Small/forced-buffered files fall back to a plain buffered File.
pub struct DestWriter {
    file: File,
    direct: bool,
    tail: Vec<u8>,        // linux O_DIRECT only: holds the sub-4K final tail
    aligned: Option<AlignedBuffer>,
}

impl DestWriter {
    // Preallocates to the final size so writes never extend the file: per-write EOF
    // extension costs a synchronous metadata/journal commit under write-through,
    // which profiling showed doubles disk busy-time and causes audible seeking.
    pub fn create(path: &Path, file_size: u64) -> io::Result<Self> {
        let w = Self::open(path, file_size)?;
        if file_size > 0 {
            w.file.set_len(file_size)?;
        }
        Ok(w)
    }

    fn open(path: &Path, file_size: u64) -> io::Result<Self> {
        if file_size < DIRECT_WRITE_THRESHOLD || buffered_writes_forced() {
            let file = File::create(path)?;
            return Ok(Self { file, direct: false, tail: Vec::new(), aligned: None });
        }

        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_FLAG_WRITE_THROUGH: no alignment requirement, plain write_all path.
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .custom_flags(0x80000000)
                .open(path)?;
            return Ok(Self { file, direct: false, tail: Vec::new(), aligned: None });
        }

        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;
            let file = File::create(path)?;
            if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) } == -1 {
                return Err(io::Error::last_os_error());
            }
            return Ok(Self { file, direct: false, tail: Vec::new(), aligned: None });
        }

        #[cfg(all(unix, not(target_os = "macos")))]
        {
            use std::os::unix::fs::OpenOptionsExt;
            match std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .custom_flags(libc::O_DIRECT)
                .open(path)
            {
                Ok(file) => Ok(Self {
                    file,
                    direct: true,
                    tail: Vec::new(),
                    aligned: Some(AlignedBuffer::new(BUFFER_SIZE)),
                }),
                // Some filesystems (tmpfs) reject O_DIRECT: fall back to buffered.
                Err(_) => {
                    let file = File::create(path)?;
                    Ok(Self { file, direct: false, tail: Vec::new(), aligned: None })
                }
            }
        }
    }

    pub fn write_chunk(&mut self, data: &[u8]) -> io::Result<()> {
        if !self.direct {
            return self.file.write_all(data);
        }
        // read_full guarantees every chunk but the last is BUFFER_SIZE (4K-aligned), so
        // a stashed tail can only precede the final chunk; anything after it is a bug.
        if !self.tail.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unaligned chunk before final chunk",
            ));
        }
        let full = data.len() & !4095;
        let aligned = self.aligned.as_mut().unwrap();
        aligned.as_mut_slice()[..full].copy_from_slice(&data[..full]);
        self.file.write_all(&aligned.as_mut_slice()[..full])?;
        self.tail.extend_from_slice(&data[full..]);
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<()> {
        if self.direct && !self.tail.is_empty() {
            // Clear O_DIRECT so the sub-4K tail can be written without alignment.
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                use std::os::fd::AsRawFd;
                let fd = self.file.as_raw_fd();
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                if flags == -1 {
                    return Err(io::Error::last_os_error());
                }
                if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_DIRECT) } == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            self.file.write_all(&self.tail)?;
        }
        self.file.sync_all()
    }
}
