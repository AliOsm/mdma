//! Kernel-backed executable identity. No current_exe/path-only fallback.
#[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
use std::{fs::File, io};

#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // dyld/libproc FFI
mod macos;

#[cfg(target_os = "freebsd")]
#[allow(unsafe_code)] // sysctl FFI
mod freebsd;

#[cfg(target_os = "linux")]
pub(super) fn open() -> io::Result<File> {
    File::open("/proc/self/exe")
}

#[cfg(target_os = "linux")]
pub(super) fn validate(file: &File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let current = std::fs::metadata("/proc/self/exe")?;
    let pinned = file.metadata()?;
    if !pinned.is_file() || current.dev() != pinned.dev() || current.ino() != pinned.ino() {
        return Err(io::Error::other(
            "executable descriptor does not match the running image",
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) use macos::{open, validate};

#[cfg(target_os = "freebsd")]
pub(super) use freebsd::{open, validate};

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
pub(super) fn open() -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "running executable identity is unsupported on this platform",
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
pub(super) fn validate(_: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "running executable identity is unsupported on this platform",
    ))
}
