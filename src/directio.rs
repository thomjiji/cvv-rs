use std::fs::File;
use std::io;
use std::path::Path;

pub const BUFFER_SIZE: usize = 8 * 1024 * 1024;

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
