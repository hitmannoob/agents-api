use anyhow::Context;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use serde::Serialize;
use std::fs::File;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::Write;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;
use tokio::process::Child;
use tokio::process::Command;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 10);
const RECLAIM_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 10);
/// File in the worker home that records which process currently owns it, so a
/// worker orphaned by an API-process crash can be found and reclaimed.
const WORKER_RECORD: &str = "agents-api-worker.json";
#[cfg(any(target_os = "linux", target_os = "macos"))]
const WORKER_INTENT: &str = "agents-api-worker-intent.json";

/// Written before fork. The private, randomly named socket distinguishes this
/// spawn from any other process, including a reused PID.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Serialize, Deserialize)]
struct SpawnIntent {
    command: String,
}

/// Durable identity of the managed worker owning a home. The PID alone is
/// insufficient to reclaim an orphan because PIDs are reused; `start_time`
/// authenticates that a live PID is still the same process we spawned.
#[derive(Serialize, Deserialize)]
struct OwnershipRecord {
    pid: u32,
    start_time: u64,
}

/// Liveness and identity of a probed process. `zombie` distinguishes a process
/// still executing from one that has exited but is not yet reaped; a zombie no
/// longer runs and so no longer touches the home.
struct ProcessInfo {
    start_time: u64,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    start_time_subsecond: u64,
    zombie: bool,
}

/// Owns one local harness process, its private socket, and its persistent-home lock.
pub(crate) struct Worker {
    child: Child,
    socket: AbsolutePathBuf,
    _socket_directory: tempfile::TempDir,
    _home_lock: File,
}

impl Worker {
    pub(crate) async fn spawn(
        executable: AbsolutePathBuf,
        home: AbsolutePathBuf,
    ) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(&home).await?;
        let home_lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(home.join("agents-api-worker.lock"))?;
        home_lock
            .try_lock()
            .context("worker home is already in use")?;
        reclaim_orphan(&home).await?;
        // Keep the socket path short even when the persistent data path is deep.
        let socket_directory = tempfile::Builder::new().prefix("agents-").tempdir()?;
        let private_directory = socket_directory.path().join("private");
        codex_uds::prepare_private_socket_directory(&private_directory).await?;
        let socket = AbsolutePathBuf::try_from(private_directory.join("worker.sock"))?;
        let mut command = Command::new(executable.as_path());
        command
            .arg("--listen")
            .arg(format!("unix://{}", socket.display()))
            .arg("--strict-config")
            .env("CODEX_HOME", home.as_path())
            .current_dir(home.as_path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        // The worker runs model-driven work, so it must not inherit the
        // service's own secrets.
        for name in [
            "CODEX_AGENTS_API_TOKEN",
            "CODEX_AGENTS_API_VAULT_PASSPHRASE",
            "CODEX_AGENTS_API_ENVIRONMENT_KEY",
        ] {
            command.env_remove(name);
        }
        #[cfg(windows)]
        command.env(codex_app_server_transport::DAEMON_SHUTDOWN_SOCKET_ENV, "1");
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let intent = {
            let intent = write_record(
                &home,
                WORKER_INTENT,
                &SpawnIntent {
                    command: format!(
                        "{} --listen unix://{} --strict-config",
                        executable.display(),
                        socket.display()
                    ),
                },
            )?;
            intent.try_lock()?;
            // Unix socket mode does not read stdin. Inheriting this locked file
            // keeps recovery waiting even if the owner dies during fork/exec,
            // before the worker's identifying argv becomes visible.
            command.stdin(Stdio::from(intent.try_clone()?));
            intent
        };
        let child = command
            .spawn()
            .with_context(|| format!("failed to start app-server at {}", executable.display()))?;
        #[cfg(test)]
        if let Some(path) = std::env::var_os("CODEX_AGENTS_API_TEST_SPAWN_WINDOW_PID") {
            std::fs::write(path, child.id().context("worker process ID")?.to_string())?;
            loop {
                std::thread::park();
            }
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let pid = child.id().context("worker process ID")?;
            let info = probe(pid).context("failed to identify spawned worker")?;
            write_record(
                &home,
                WORKER_RECORD,
                &OwnershipRecord {
                    pid,
                    start_time: info.start_time,
                },
            )?;
            drop(intent);
        }
        Ok(Self {
            child,
            socket,
            _socket_directory: socket_directory,
            _home_lock: home_lock,
        })
    }

    pub(crate) fn id(&self) -> Option<u32> {
        self.child.id()
    }

    pub(crate) fn socket(&self) -> &AbsolutePathBuf {
        &self.socket
    }

    pub(crate) async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.child.wait().await
    }

    pub(crate) async fn shutdown(mut self) -> anyhow::Result<()> {
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        let pid = self.child.id().context("worker has no process ID")?;
        let graceful = async {
            #[cfg(unix)]
            {
                let pid = i32::try_from(pid)?;
                // SAFETY: this PID belongs to our live, unreaped Child.
                if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            #[cfg(windows)]
            {
                use futures::SinkExt;
                use futures::StreamExt;
                use tokio_tungstenite::tungstenite::Message;

                let stream = codex_uds::UnixStream::connect(self.socket.as_path()).await?;
                stream.ensure_non_elevated_peer()?;
                let (mut socket, _) =
                    tokio_tungstenite::client_async("ws://localhost/daemon/shutdown", stream)
                        .await?;
                socket.send(Message::Text(pid.to_string().into())).await?;
                let reply = socket
                    .next()
                    .await
                    .context("missing shutdown acknowledgment")??;
                anyhow::ensure!(
                    matches!(reply, Message::Text(ack) if ack == pid.to_string()),
                    "shutdown acknowledgment did not match worker"
                );
                socket.close(None).await?;
            }
            self.child.wait().await?;
            anyhow::Ok(())
        };
        match tokio::time::timeout(SHUTDOWN_TIMEOUT, graceful).await {
            Ok(Ok(())) => Ok(()),
            result => {
                if self.child.try_wait()?.is_some() {
                    return Ok(());
                }
                tracing::warn!(?result, "forcing worker shutdown");
                self.child
                    .kill()
                    .await
                    .context("failed to stop and reap worker")
            }
        }
    }
}

pub(crate) async fn connect(
    socket: AbsolutePathBuf,
    deadline: Duration,
) -> anyhow::Result<RemoteAppServerClient> {
    let mut last_error = None;
    let connecting = async {
        loop {
            match RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
                endpoint: RemoteAppServerEndpoint::UnixSocket {
                    socket_path: socket.clone(),
                },
                client_name: "codex_agents_api".into(),
                client_version: env!("CARGO_PKG_VERSION").into(),
                experimental_api: true,
                mcp_server_openai_form_elicitation: false,
                opt_out_notification_methods: vec![],
                channel_capacity: 128,
            })
            .await
            {
                Ok(client) => return client,
                Err(error) => last_error = Some(error),
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 50)).await;
        }
    };
    tokio::time::timeout(deadline, connecting)
        .await
        .with_context(|| {
            format!(
                "app-server initialization timed out at {}: {last_error:?}",
                socket.display()
            )
        })
}

pub(crate) async fn worker_exit(worker: &mut Option<Worker>) -> anyhow::Result<()> {
    match worker {
        Some(worker) => anyhow::bail!("managed app-server exited: {}", worker.wait().await?),
        None => std::future::pending().await,
    }
}

/// Reclaim a worker orphaned by an API-process crash before starting a new one.
///
/// A crash (for example SIGKILL of the API) skips graceful shutdown, so the
/// managed worker keeps running against this home with no owner and its home
/// lock is released; starting a second worker would then let two app-servers
/// write one home. New workers inherit a locked spawn intent; recovery matches
/// its unique command and authenticates the process start time before signaling.
/// Legacy records use PID and start time. The caller holds the home lock, so a
/// live owner cannot be reclaimed. External workers are never recorded or touched.
pub(crate) async fn reclaim_orphan(home: &AbsolutePathBuf) -> anyhow::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if home.join(WORKER_INTENT).exists() {
        reclaim_intent(home).await?;
        std::fs::remove_file(home.join(WORKER_RECORD)).or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(error)
            }
        })?;
        std::fs::remove_file(home.join(WORKER_INTENT))?;
        return Ok(());
    }
    let record_path = home.join(WORKER_RECORD);
    let Ok(bytes) = std::fs::read(record_path.as_path()) else {
        return Ok(());
    };
    if let Ok(record) = serde_json::from_slice::<OwnershipRecord>(&bytes)
        && probe(record.pid)
            .is_some_and(|info| !info.zombie && info.start_time == record.start_time)
    {
        tracing::warn!(
            pid = record.pid,
            "reclaiming worker orphaned by a prior API crash"
        );
        terminate(record.pid)?;
        let deadline = Instant::now() + RECLAIM_TIMEOUT;
        while Instant::now() < deadline
            && probe(record.pid)
                .is_some_and(|info| !info.zombie && info.start_time == record.start_time)
        {
            tokio::time::sleep(Duration::from_millis(/*millis*/ 50)).await;
        }
        anyhow::ensure!(
            !probe(record.pid)
                .is_some_and(|info| !info.zombie && info.start_time == record.start_time),
            "orphaned worker did not exit"
        );
    }
    let _ = std::fs::remove_file(record_path.as_path());
    Ok(())
}

/// Atomically publish a synced record, so an API crash cannot leave partial JSON.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_record(
    home: &AbsolutePathBuf,
    name: &str,
    record: &impl Serialize,
) -> anyhow::Result<File> {
    let mut file = tempfile::NamedTempFile::new_in(home)?;
    file.write_all(&serde_json::to_vec(record)?)?;
    file.as_file().sync_all()?;
    let file = file.persist(home.join(name))?;
    File::open(home)?.sync_all()?;
    Ok(file)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn reclaim_intent(home: &AbsolutePathBuf) -> anyhow::Result<()> {
    let file = File::options()
        .read(true)
        .write(true)
        .open(home.join(WORKER_INTENT))?;
    let intent: SpawnIntent = serde_json::from_reader(&file)?;
    let deadline = Instant::now() + RECLAIM_TIMEOUT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(error) => return Err(error.into()),
        }
        // A child may not have exec'd yet. Its inherited lock prevents a new
        // spawn until its unique argv appears, or it exits and releases the lock.
        for pid in processes_with_command(&intent.command, deadline).await? {
            if let Some(info) = probe(pid)
                && !info.zombie
                && processes_with_command(&intent.command, deadline)
                    .await?
                    .contains(&pid)
                && probe(pid).is_some_and(|current| {
                    current.start_time == info.start_time
                        && current.start_time_subsecond == info.start_time_subsecond
                })
            {
                tracing::warn!(pid, "reclaiming worker from spawn intent");
                terminate(pid)?;
            }
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "spawn intent is still in use; refusing to start another worker"
        );
        tokio::time::sleep(Duration::from_millis(/*millis*/ 50)).await;
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn processes_with_command(command: &str, deadline: Instant) -> anyhow::Result<Vec<u32>> {
    let output = tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        Command::new("/bin/ps")
            .args(["-axww", "-o", "pid=", "-o", "command="])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("spawn intent is still in use; process inspection timed out")??;
    anyhow::ensure!(
        output.status.success(),
        "failed to inspect worker process identity"
    );
    Ok(String::from_utf8(output.stdout)?
        .lines()
        .filter_map(|line| {
            let (pid, args) = line.trim_start().split_once(char::is_whitespace)?;
            let pid = pid.parse().ok()?;
            (args.trim_start() == command).then_some(pid)
        })
        .collect())
}

/// Read a process's start time and whether it has become a zombie, or `None`
/// when no such process exists. Start time authenticates against PID reuse.
#[cfg(target_os = "linux")]
fn probe(pid: u32) -> Option<ProcessInfo> {
    // /proc/<pid>/stat: fields after the final ')' start at the state field;
    // start time is field 22 overall, i.e. index 19 among those fields.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    Some(ProcessInfo {
        start_time: fields.get(/*starttime*/ 19)?.parse().ok()?,
        start_time_subsecond: 0,
        zombie: fields.first().is_some_and(|state| *state == "Z"),
    })
}

#[cfg(target_os = "macos")]
fn probe(pid: u32) -> Option<ProcessInfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: fills a zeroed proc_bsdinfo of the size passed for the given pid.
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            /*arg*/ 0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    (written == size).then_some(ProcessInfo {
        start_time: info.pbi_start_tvsec,
        start_time_subsecond: info.pbi_start_tvusec,
        zombie: info.pbi_status == libc::SZOMB,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn probe(_pid: u32) -> Option<ProcessInfo> {
    // Without an authenticated probe this platform cannot safely reclaim an
    // orphan; startup falls back to the home lock alone.
    None
}

#[cfg(unix)]
fn terminate(pid: u32) -> anyhow::Result<()> {
    let pid = i32::try_from(pid)?;
    anyhow::ensure!(pid > 0, "invalid worker process ID");
    // SAFETY: sends SIGKILL to a PID authenticated as our orphaned worker.
    if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error.into());
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn terminate(_pid: u32) -> anyhow::Result<()> {
    Ok(())
}
