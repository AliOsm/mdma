use super::*;
use std::io::Write;
use tokio_util::sync::CancellationToken;

fn startup_config() -> crate::config::Config {
    crate::config::Config::load(|name| match name {
        "MDMA_MODELS_DIR" | "MDMA_TLS_CERT" | "MDMA_TLS_KEY" | "MDMA_TLS_CA" => {
            Ok("/unused-startup-fixture".into())
        }
        "MDMA_LISTEN" => Ok("127.0.0.1:0".into()),
        "MDMA_POWER" => Ok("off".into()),
        _ => Err(std::env::VarError::NotPresent),
    })
    .unwrap()
}

#[tokio::test]
async fn listener_completion_still_waits_for_workers() {
    let trigger = CancellationToken::new();
    let shutdown = Shutdown::new(trigger.clone().cancelled_owned());
    let worker = shutdown.guard();
    trigger.cancel();
    let drain = supervise(shutdown, async { Ok(()) }, Duration::from_secs(2));
    tokio::pin!(drain);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut drain)
            .await
            .is_err()
    );
    drop(worker);
    drain.await.unwrap();
}

#[tokio::test]
async fn stuck_worker_hits_the_deadline() {
    let shutdown = Shutdown::new(async {});
    let _worker = shutdown.guard();
    let result = supervise(shutdown, std::future::pending(), Duration::from_millis(20)).await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("shutdown deadline exceeded")
    );
}

#[tokio::test]
async fn startup_failure_is_returned_without_waiting_for_a_signal() {
    let shutdown = Shutdown::new(std::future::pending::<()>());
    let result = supervise(
        shutdown,
        async { Err("bind failed".into()) },
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(result.unwrap_err().to_string(), "bind failed");
}

#[tokio::test]
async fn backend_initialization_failure_is_returned() {
    let shutdown = Shutdown::new(std::future::pending::<()>());
    let guard = shutdown.guard();
    let server = crate::run::<mdma::backend::cpu::CpuBackend>(
        || Err(mdma::backend::Error::Backend("driver unavailable".into())),
        "cpu",
        startup_config(),
        guard,
    );
    let result = supervise(shutdown, server, Duration::from_secs(2)).await;
    assert!(result.unwrap_err().to_string().contains("driver unavailable"));
}

#[cfg(unix)]
#[test]
fn unix_signals_and_blocked_runtime_exit() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::time::Instant;

    for (signal, mode) in [
        ("-TERM", "idle"),
        ("-INT", "idle"),
        ("-TERM", "blocked"),
        ("-TERM", "initializing"),
        ("-INT", "initializing"),
    ] {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "lifecycle::tests::signal_child",
                "--ignored",
                "--nocapture",
            ])
            .env("MDMA_TEST_SIGNAL_CHILD", mode)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if line.unwrap().contains("SIGNALS_READY") {
                    let _ = ready_tx.send(());
                    break;
                }
            }
        });
        let ready = ready_rx.recv_timeout(Duration::from_secs(10));
        if ready.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        ready.unwrap();
        reader.join().unwrap();
        assert!(
            Command::new("/bin/kill")
                .args([signal, &child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{mode} child did not stop after {signal}");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(i32::from(mode != "idle")), "{mode}");
    }
}

#[cfg(unix)]
#[test]
#[ignore = "subprocess fixture for unix_signals_and_blocked_runtime_exit"]
fn signal_child() {
    let Ok(mode) = std::env::var("MDMA_TEST_SIGNAL_CHILD") else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        let shutdown = Shutdown::new(shutdown_signal().unwrap());
        if mode == "initializing" {
            let guard = shutdown.guard();
            let server = crate::run(
                || {
                    println!("SIGNALS_READY");
                    std::io::stdout().flush().unwrap();
                    std::thread::sleep(Duration::from_secs(60));
                    Ok(mdma::backend::cpu::CpuBackend)
                },
                "cpu",
                startup_config(),
                guard,
            );
            return supervise(shutdown, server, Duration::from_millis(50)).await;
        }
        if mode == "blocked" {
            let guard = shutdown.guard();
            tokio::task::spawn_blocking(move || {
                let _guard = guard;
                std::thread::sleep(Duration::from_secs(60));
            });
        }
        println!("SIGNALS_READY");
        std::io::stdout().flush().unwrap();
        supervise(shutdown, std::future::pending(), Duration::from_millis(50)).await
    });
    runtime.shutdown_timeout(Duration::ZERO);
    std::process::exit(i32::from(result.is_err()));
}
