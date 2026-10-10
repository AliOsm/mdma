use super::*;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};

#[test]
fn excludes_second_owner_and_releases_without_deleting_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lock");
    let first = MachineLock::at(&path).unwrap();
    assert_eq!(
        MachineLock::at(&path).err().unwrap().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(first);
    assert!(path.exists());
    let _next = MachineLock::at(&path).unwrap();
}

#[test]
fn child_lock() {
    let Some(path) = std::env::var_os("MDMA_TEST_CHILD_LOCK") else {
        return;
    };
    let _lock = MachineLock::at(Path::new(&path)).unwrap();
    println!("LOCKED");
    // Parent keeps stdin open, then kills us to test kernel cleanup.
    let _ = io::stdin().read_to_end(&mut Vec::new());
}

#[test]
fn process_exit_releases_lock_without_stale_pid_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lock");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "singleton::tests::child_lock", "--nocapture"])
        .env("MDMA_TEST_CHILD_LOCK", &path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        assert!(
            output.read_line(&mut line).unwrap() > 0,
            "child exited before locking"
        );
        if line.trim() == "LOCKED" {
            break;
        }
        line.clear();
    }
    let blocked = MachineLock::at(&path).err().map(|error| error.kind());
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(blocked, Some(io::ErrorKind::WouldBlock));
    let _restarted = MachineLock::at(&path).unwrap();
}
