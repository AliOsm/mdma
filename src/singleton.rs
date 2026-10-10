//! One cooperating MDMA daemon per machine, independent of user and port.
//! Never unlink the lock file: replacing its inode would allow two owners.
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// A kernel-owned lock, released on drop or process exit (including SIGKILL).
pub struct MachineLock {
    file: File,
}

impl Drop for MachineLock {
    fn drop(&mut self) {
        // Explicit unlock also avoids transient inherited descriptors in a
        // concurrently forked child keeping the lock alive until exec.
        if let Err(error) = self.file.unlock() {
            eprintln!("mdma: machine lock unlock failed; closing the descriptor releases it: {error}");
        }
    }
}

impl MachineLock {
    /// Fixed machine-wide location, not `$TMPDIR`, `$HOME` or an env override.
    pub fn acquire() -> io::Result<Self> {
        Self::at(Path::new("/tmp/mdma-machine.lock"))
    }

    fn at(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => io::Error::new(
                io::ErrorKind::WouldBlock,
                "another MDMA daemon already owns this machine",
            ),
            std::fs::TryLockError::Error(error) => error,
        })?;
        Ok(Self { file })
    }
}

#[cfg(test)]
mod tests;
