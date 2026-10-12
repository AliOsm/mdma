//! Child processes for end-to-end tests.
use std::process::{Child, Command};

/// Killed and reaped when dropped.
pub struct Process(pub Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `binary` without the caller's MDMA_* environment.
pub fn command(binary: &str) -> Command {
    let mut command = Command::new(binary);
    for (key, _) in
        std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("MDMA_"))
    {
        command.env_remove(key);
    }
    command
}
