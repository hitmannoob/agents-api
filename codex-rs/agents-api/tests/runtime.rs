// Exercise the private process owner without expanding the library's public API.
#[path = "../src/runtime.rs"]
mod runtime;

use anyhow::Context;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_cargo_bin::cargo_bin;
use serde_json::Value;
use serde_json::json;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

const DEADLINE: Duration = Duration::from_secs(/*secs*/ 30);

async fn worker(home: &std::path::Path) -> anyhow::Result<runtime::Worker> {
    runtime::Worker::spawn(
        AbsolutePathBuf::try_from(cargo_bin("codex-app-server")?)?,
        AbsolutePathBuf::from_absolute_path(home)?,
    )
    .await
}

#[tokio::test]
async fn managed_worker_initializes_locks_home_and_cleans_up() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let owned = worker(home.path()).await?;
    assert!(owned.id().is_some());
    let socket = owned.socket().clone();
    let client = runtime::connect(socket.clone(), DEADLINE).await?;
    assert!(client.server_version().is_some());
    assert!(worker(home.path()).await.is_err());
    client.shutdown().await?;
    owned.shutdown().await?;
    assert!(!socket.exists());
    // A subsequent owner can reuse the same persistent home.
    worker(home.path()).await?.shutdown().await?;
    assert!(home.path().exists());
    Ok(())
}

#[tokio::test]
async fn startup_exit_is_observed_and_reaped() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    std::fs::write(home.path().join("config.toml"), "invalid = [")?;
    let mut owned = Some(worker(home.path()).await?);
    let error = tokio::time::timeout(DEADLINE, runtime::worker_exit(&mut owned))
        .await?
        .expect_err("invalid configuration must exit");
    assert!(error.to_string().contains("managed app-server exited"));
    owned.context("owned worker")?.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn readiness_deadline_includes_a_stalled_handshake() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let private = directory.path().join("private");
    codex_uds::prepare_private_socket_directory(&private).await?;
    let socket = AbsolutePathBuf::try_from(private.join("worker.sock"))?;
    let mut listener = codex_uds::UnixListener::bind(&socket).await?;
    let connecting = runtime::connect(socket, Duration::from_millis(/*millis*/ 200));
    let stalled = async {
        let _peer = listener.accept().await?;
        std::future::pending::<std::io::Result<()>>().await
    };
    let result = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        tokio::select! {
            result = connecting => result.map(|_| ()),
            result = stalled => result.map_err(Into::into),
        }
    })
    .await?;
    assert!(
        result
            .expect_err("initialization must time out")
            .to_string()
            .contains("initialization timed out")
    );
    Ok(())
}

#[tokio::test]
async fn missing_worker_executable_is_reported() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let home = AbsolutePathBuf::from_absolute_path(home.path())?;
    let error = runtime::Worker::spawn(home.join("missing-worker"), home)
        .await
        .err()
        .context("missing executable should fail")?;
    assert!(error.to_string().contains("failed to start app-server"));
    Ok(())
}

// Run in a subprocess so SIGKILL skips both Worker::drop and the record write.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn spawn_window_owner() -> anyhow::Result<()> {
    if let Some(home) = std::env::var_os("CODEX_AGENTS_API_TEST_SPAWN_WINDOW_HOME") {
        worker(std::path::Path::new(&home))
            .await?
            .shutdown()
            .await?;
        anyhow::bail!("spawn window hook did not stop the owner");
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn worker_spawned_before_record_is_reclaimed_after_owner_crash() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let pid_path = home.path().join("spawned-pid");
    let mut owner = Command::new(std::env::current_exe()?)
        .args(["--exact", "spawn_window_owner", "--nocapture"])
        .env("CODEX_AGENTS_API_TEST_SPAWN_WINDOW_HOME", home.path())
        .env("CODEX_AGENTS_API_TEST_SPAWN_WINDOW_PID", &pid_path)
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let pid: u32 = tokio::time::timeout(DEADLINE, async {
        loop {
            if let Ok(pid) = std::fs::read_to_string(&pid_path)
                && let Ok(pid) = pid.parse()
            {
                return anyhow::Ok(pid);
            }
            anyhow::ensure!(
                owner.try_wait()?.is_none(),
                "spawn window owner exited early"
            );
            tokio::time::sleep(Duration::from_millis(/*millis*/ 50)).await;
        }
    })
    .await??;
    assert!(!home.path().join("agents-api-worker.json").exists());
    let intent: Value = serde_json::from_slice(&std::fs::read(
        home.path().join("agents-api-worker-intent.json"),
    )?)?;
    let (_, socket) = intent["command"]
        .as_str()
        .context("spawn command")?
        .strip_suffix(" --strict-config")
        .context("strict config")?
        .split_once(" --listen unix://")
        .context("worker socket")?;
    let socket = AbsolutePathBuf::try_from(std::path::PathBuf::from(socket))?;
    runtime::connect(socket.clone(), DEADLINE)
        .await?
        .shutdown()
        .await?;
    owner.kill().await?;
    // It really outlived its owner with no completed ownership record.
    runtime::connect(socket.clone(), DEADLINE)
        .await?
        .shutdown()
        .await?;
    let replacement = worker(home.path()).await?;
    pretty_assertions::assert_ne!(replacement.id(), Some(pid));
    runtime::connect(replacement.socket().clone(), DEADLINE)
        .await?
        .shutdown()
        .await?;
    assert!(
        runtime::connect(socket, Duration::from_millis(/*millis*/ 200))
            .await
            .is_err()
    );
    replacement.shutdown().await?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn pid_reused_record_does_not_signal_foreign_process() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let mut foreign = Command::new("/bin/sleep")
        .arg("30")
        .kill_on_drop(true)
        .spawn()?;
    std::fs::write(
        home.path().join("agents-api-worker.json"),
        serde_json::to_vec(&json!({
            "pid": foreign.id().context("foreign PID")?,
            "start_time": u64::MAX,
        }))?,
    )?;
    let replacement = worker(home.path()).await?;
    assert!(
        foreign.try_wait()?.is_none(),
        "a reused PID must not be signaled"
    );
    replacement.shutdown().await?;
    foreign.kill().await?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn stale_spawn_intent_does_not_signal_another_worker() -> anyhow::Result<()> {
    let foreign_home = tempfile::tempdir()?;
    let foreign = worker(foreign_home.path()).await?;
    let home = tempfile::tempdir()?;
    // Even an authentic PID/start time is insufficient when it belongs to a
    // different spawn. The stale intent's unique command does not match it.
    std::fs::copy(
        foreign_home.path().join("agents-api-worker.json"),
        home.path().join("agents-api-worker.json"),
    )?;
    std::fs::write(
        home.path().join("agents-api-worker-intent.json"),
        serde_json::to_vec(&json!({
            "command": "codex-app-server --listen unix:///missing-worker.sock --strict-config",
        }))?,
    )?;
    let intent = std::fs::File::options()
        .read(true)
        .write(true)
        .open(home.path().join("agents-api-worker-intent.json"))?;
    intent.try_lock()?;
    let error = worker(home.path())
        .await
        .err()
        .context("unidentified locked intent must prevent another spawn")?;
    assert!(error.to_string().contains("spawn intent is still in use"));
    runtime::connect(foreign.socket().clone(), DEADLINE)
        .await?
        .shutdown()
        .await?;
    drop(intent);
    let replacement = worker(home.path()).await?;
    runtime::connect(foreign.socket().clone(), DEADLINE)
        .await?
        .shutdown()
        .await?;
    replacement.shutdown().await?;
    foreign.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
#[path = "suite/runtime_cli.rs"]
mod runtime_cli;

#[cfg(unix)]
#[path = "suite/worker_loss.rs"]
mod worker_loss;

#[cfg(unix)]
#[path = "suite/reconcile_recovery.rs"]
mod reconcile_recovery;
