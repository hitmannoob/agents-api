mod exporters;
mod runtime;

use anyhow::Context;
use clap::Parser;
use codex_agents_api::AgentsApi;
use codex_app_server_client::AppServerClient;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::StreamExt;
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

#[derive(Parser)]
#[command(subcommand_negates_reqs = true)]
struct Args {
    /// Manage projects and API keys instead of serving.
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long, default_value = "127.0.0.1:4501")]
    listen: SocketAddr,
    /// Public http(s) origin advertised to self-hosted executors.
    #[arg(long, env = "CODEX_AGENTS_API_PUBLIC_URL")]
    public_url: Option<String>,
    /// Connect to a caller-owned worker instead of starting a managed worker.
    #[arg(long, conflicts_with_all = ["app_server_bin", "codex_home"])]
    app_server_socket: Option<PathBuf>,
    /// Managed worker executable; defaults to the sibling codex-app-server binary.
    #[arg(long)]
    app_server_bin: Option<PathBuf>,
    /// Persistent worker configuration/history; defaults to DATA_DIRECTORY/codex-home.
    #[arg(long)]
    codex_home: Option<PathBuf>,
    #[arg(long, default_value_t = 30)]
    worker_startup_timeout_secs: u64,
    /// Managed workers to run (run mode's pool; see RUN_MODE.md). Each
    /// session is assigned to one on its first input; 1 serves every
    /// session on a single worker.
    #[arg(
        long,
        default_value_t = 1,
        value_parser = clap::value_parser!(u16).range(1..=codex_agents_api::MAX_WORKERS as i64),
        conflicts_with = "app_server_socket"
    )]
    workers: u16,
    /// Milliseconds between one pool worker becoming ready and the next one
    /// starting.
    #[arg(long, default_value_t = 1000)]
    worker_start_stagger_ms: u64,
    #[arg(long, required = true)]
    data_directory: Option<PathBuf>,
    /// Operator token. It authenticates as the default project with every
    /// permission.
    #[arg(
        long,
        env = "CODEX_AGENTS_API_TOKEN",
        hide_env_values = true,
        required = true
    )]
    token: Option<String>,
    /// MCP server host the API may reach even when it is internal or plain
    /// http. Repeat for each host.
    #[arg(long = "allow-mcp-host")]
    allow_mcp_hosts: Vec<String>,
    /// Webhook receiver host the API may deliver to even when it is internal
    /// or plain http. Repeat for each host.
    #[arg(long = "allow-webhook-host")]
    allow_webhook_hosts: Vec<String>,
    /// Input image host the API may fetch from even when it is internal or
    /// plain http. Repeat for each host.
    #[arg(long = "allow-image-host")]
    allow_image_hosts: Vec<String>,
    /// Passphrase that encrypts vault credentials in the data directory.
    /// Vault credentials are unavailable without it.
    #[arg(
        long,
        env = "CODEX_AGENTS_API_VAULT_PASSPHRASE",
        hide_env_values = true
    )]
    vault_passphrase: Option<String>,
    /// Key that self-hosted executors present to this service's environment
    /// registry (as their CODEX_API_KEY). It grants only the registry, never
    /// the API. Self-hosted environments are unavailable without it.
    #[arg(long, env = "CODEX_AGENTS_API_ENVIRONMENT_KEY", hide_env_values = true)]
    environment_key: Option<String>,
    /// Program that provisions `openai_hosted` environments, speaking JSON
    /// lines on stdin and stdout (such as `deploy/modal/provider.py`). Hosted
    /// environments are unavailable without it; they also need the
    /// environment key and a public URL the provisioned executors can reach.
    #[arg(long, requires = "environment_key")]
    environment_provider: Option<String>,
    /// An argument for the environment provider. Repeat for each argument.
    #[arg(
        long = "environment-provider-arg",
        requires = "environment_provider",
        allow_hyphen_values = true
    )]
    environment_provider_args: Vec<String>,
    /// Seconds an idle `openai_hosted` environment keeps its sandbox before it
    /// is snapshotted and suspended; the session's next input resumes it.
    #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..))]
    environment_idle_suspend_secs: u64,
    /// Seconds `openai_hosted` snapshots are kept; a suspended environment
    /// expires with its snapshot.
    #[arg(long, default_value_t = 7 * 24 * 60 * 60, value_parser = clap::value_parser!(u64).range(60..))]
    environment_snapshot_ttl_secs: u64,
    /// Continue a session's turn that a worker crash or restart interrupted,
    /// for up to this many consecutive interruptions; 0 leaves such sessions
    /// failed.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(0..=10))]
    continue_interrupted_turns: u32,
    /// Seconds that input waits for a disconnected self-hosted executor before
    /// it is dropped and the session fails.
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=86_400))]
    environment_connection_wait_secs: u64,
    /// OTLP/HTTP collector base URL (for example `http://localhost:4318`) that
    /// receives the API's spans and metrics. Nothing is exported without it.
    #[arg(long, env = "CODEX_AGENTS_API_OTLP_ENDPOINT")]
    otlp_endpoint: Option<String>,
    /// Header sent with each export, as NAME=VALUE, such as a collector key.
    /// Repeat for each header.
    #[arg(long = "otlp-header", value_parser = exporters::header)]
    otlp_headers: Vec<(String, String)>,
    /// `deployment.environment` reported with exported telemetry.
    #[arg(long, default_value = "production")]
    otel_environment: String,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Create and list projects.
    Projects {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Create, list, and revoke project API keys.
    Keys {
        #[command(subcommand)]
        command: KeyCommand,
    },
}

#[derive(clap::Subcommand)]
enum ProjectCommand {
    /// Create a project and print it as JSON.
    Create {
        #[arg(long)]
        data_directory: PathBuf,
        #[arg(long)]
        name: String,
    },
    /// Print every project as JSON, oldest first.
    List {
        #[arg(long)]
        data_directory: PathBuf,
    },
}

#[derive(clap::Subcommand)]
enum KeyCommand {
    /// Create a key and print it as JSON. The secret `value` is shown only once.
    Create {
        #[arg(long)]
        data_directory: PathBuf,
        #[arg(long)]
        project: String,
        #[arg(long, default_value = "")]
        name: String,
        /// A permission the key holds. Repeat for each permission.
        #[arg(long = "permission", required_unless_present = "all_permissions")]
        permissions: Vec<String>,
        /// Grant every permission.
        #[arg(long, conflicts_with = "permissions")]
        all_permissions: bool,
    },
    /// Print keys as JSON, oldest first, without their secrets.
    List {
        #[arg(long)]
        data_directory: PathBuf,
        /// Only this project's keys.
        #[arg(long)]
        project: Option<String>,
    },
    /// Revoke a key. Requests that present it fail from then on.
    Revoke {
        #[arg(long)]
        data_directory: PathBuf,
        id: String,
    },
}

/// Run one administration command against a data directory. It may run while
/// the server serves the same directory.
async fn administer(command: Command) -> anyhow::Result<()> {
    use codex_agents_api::projects::Admin;
    let open = |directory: PathBuf| async {
        Admin::open(AbsolutePathBuf::relative_to_current_dir(directory)?).await
    };
    let output = match command {
        Command::Projects {
            command:
                ProjectCommand::Create {
                    data_directory,
                    name,
                },
        } => serde_json::to_value(open(data_directory).await?.create_project(&name).await?)?,
        Command::Projects {
            command: ProjectCommand::List { data_directory },
        } => serde_json::to_value(open(data_directory).await?.projects().await?)?,
        Command::Keys {
            command:
                KeyCommand::Create {
                    data_directory,
                    project,
                    name,
                    permissions,
                    all_permissions,
                },
        } => {
            let permissions = if all_permissions {
                codex_agents_api::projects::PERMISSIONS
                    .iter()
                    .map(|&permission| permission.to_owned())
                    .collect()
            } else {
                permissions
            };
            serde_json::to_value(
                open(data_directory)
                    .await?
                    .create_key(&project, &name, &permissions)
                    .await?,
            )?
        }
        Command::Keys {
            command:
                KeyCommand::List {
                    data_directory,
                    project,
                },
        } => serde_json::to_value(open(data_directory).await?.keys(project.as_deref()).await?)?,
        Command::Keys {
            command: KeyCommand::Revoke { data_directory, id },
        } => {
            open(data_directory).await?.revoke_key(&id).await?;
            serde_json::json!({"id": id, "revoked": true})
        }
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(command) = args.command {
        return administer(command).await;
    }
    // Clap requires both when no subcommand is given.
    let (Some(data_directory), Some(token)) = (args.data_directory, args.token) else {
        anyhow::bail!("--data-directory and --token are required to serve");
    };
    anyhow::ensure!(
        args.listen.ip().is_loopback(),
        "initial API supports loopback listeners only"
    );
    anyhow::ensure!(
        token.len() >= 32,
        "API token must contain at least 32 bytes"
    );
    let directory = AbsolutePathBuf::relative_to_current_dir(data_directory)?;
    let telemetry = exporters::install(exporters::Export {
        endpoint: args.otlp_endpoint,
        headers: args.otlp_headers,
        environment: args.otel_environment,
        data_directory: directory.to_path_buf(),
    })?;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    let startup_timeout = Duration::from_secs(args.worker_startup_timeout_secs);
    let stagger = Duration::from_millis(args.worker_start_stagger_ms);
    // Register SIGTERM before starting the child, including during readiness.
    let stopping = shutdown_signal();
    tokio::pin!(stopping);
    // Managed workers by index; external-socket mode has none and is never
    // restarted. A slot is `None` while its worker is being replaced.
    let mut workers: Vec<Option<runtime::Worker>> = Vec::new();
    let mut pool = None;
    let serving = async {
        let mut clients = Vec::new();
        match args.app_server_socket {
            Some(socket) => {
                let socket = AbsolutePathBuf::relative_to_current_dir(socket)?;
                let starting = Instant::now();
                clients.push(tokio::select! {
                    result = runtime::connect(socket, startup_timeout) => result?,
                    result = &mut stopping => return result,
                });
                exporters::elapsed(
                    exporters::WORKER_STARTUP_DURATION,
                    starting.elapsed(),
                    &[("reason", "start")],
                );
            }
            None => {
                let executable = match args.app_server_bin {
                    Some(path) => AbsolutePathBuf::relative_to_current_dir(path)?,
                    None => AbsolutePathBuf::try_from(std::env::current_exe()?.with_file_name(
                        format!("codex-app-server{}", std::env::consts::EXE_SUFFIX),
                    ))?,
                };
                let home = match args.codex_home {
                    Some(path) => AbsolutePathBuf::relative_to_current_dir(path)?,
                    None => directory.join("codex-home"),
                };
                let managed = &*pool.insert(Pool {
                    executable,
                    home,
                    size: usize::from(args.workers),
                });
                // Workers start one at a time, each after the previous one's
                // handshake and the stagger, so startup memory peaks do not
                // stack up.
                for index in 0..managed.size {
                    if index > 0 {
                        tokio::select! {
                            () = tokio::time::sleep(stagger) => {}
                            result = &mut stopping => return result,
                        }
                    }
                    let starting = Instant::now();
                    let owned = managed.spawn(index).await?;
                    // This line and the listening line stay plain, outside
                    // `tracing`: supervisors read the worker PIDs and the
                    // address from them.
                    let pid = owned.id().context("worker process ID")?;
                    if managed.size == 1 {
                        eprintln!("agents-api managed worker pid={pid}");
                    } else {
                        eprintln!("agents-api managed worker {index} pid={pid}");
                    }
                    let socket = owned.socket().clone();
                    workers.push(Some(owned));
                    let slot = workers.last_mut().context("worker slot")?;
                    clients.push(tokio::select! {
                        result = runtime::connect(socket, startup_timeout) => result?,
                        result = runtime::worker_exit(slot) => return result,
                        result = &mut stopping => return result,
                    });
                    exporters::elapsed(
                        exporters::WORKER_STARTUP_DURATION,
                        starting.elapsed(),
                        &[("reason", "start")],
                    );
                }
            }
        }
        let clients = clients.into_iter().map(AppServerClient::Remote).collect();
        let api = AgentsApi::with_workers(clients, directory, token).await?;
        if let Some(public_url) = args.public_url {
            api.configure_public_url(public_url)?;
        }
        api.allow_mcp_hosts(args.allow_mcp_hosts);
        api.allow_webhook_hosts(args.allow_webhook_hosts);
        api.allow_image_hosts(args.allow_image_hosts);
        if let Some(passphrase) = args.vault_passphrase {
            api.configure_vault(passphrase).await?;
        }
        if let Some(environment_key) = args.environment_key {
            // The worker runs beside this process, so it reaches the registry
            // over loopback whatever address clients use.
            let mut local = listener.local_addr()?;
            if local.ip().is_unspecified() {
                local.set_ip(match local.ip() {
                    std::net::IpAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
                    std::net::IpAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
                });
            }
            api.configure_environments(environment_key, format!("http://{local}/registry"))?;
            api.set_environment_connection_wait(std::time::Duration::from_secs(
                args.environment_connection_wait_secs,
            ));
        }
        if let Some(program) = args.environment_provider {
            api.set_managed_lifecycle(codex_agents_api::managed::Lifecycle {
                idle_suspend: std::time::Duration::from_secs(args.environment_idle_suspend_secs),
                snapshot_ttl: std::time::Duration::from_secs(args.environment_snapshot_ttl_secs),
                ..Default::default()
            });
            api.configure_environment_provider(std::sync::Arc::new(
                codex_agents_api::managed::SidecarProvider::new(
                    program,
                    args.environment_provider_args,
                ),
            ));
        }
        api.set_interrupted_turn_continuations(args.continue_interrupted_turns);
        eprintln!("agents-api listening on {}", listener.local_addr()?);
        let (stop_http, stopping_http) = tokio::sync::oneshot::channel();
        let mut http = tokio::spawn(
            axum::serve(listener, api.router())
                .with_graceful_shutdown(async {
                    let _ = stopping_http.await;
                })
                .into_future(),
        );
        // Keep serving durable reads while a crashed managed worker is replaced.
        // A lost worker no longer stops the process; only http failure, restart
        // exhaustion, or a shutdown signal ends it. Each worker has its own
        // supervisor, so one worker's restart never waits on another's.
        let http_result = |result: Result<std::io::Result<()>, tokio::task::JoinError>| {
            result
                .context("HTTP server task failed")
                .and_then(|result| result.map_err(Into::into))
        };
        let result: anyhow::Result<()> = {
            let mut supervisors: futures::stream::FuturesUnordered<_> = match &pool {
                Some(managed) => workers
                    .iter_mut()
                    .enumerate()
                    .map(|(index, slot)| supervise(index, slot, &api, managed, startup_timeout))
                    .collect(),
                None => futures::stream::FuturesUnordered::new(),
            };
            tokio::select! {
                result = &mut http => http_result(result),
                result = &mut stopping => result,
                // A supervisor returns only when its worker cannot be restarted.
                Some(result) = supervisors.next() => result,
            }
        };
        let _ = stop_http.send(());
        let cleanup = tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
            let cleanup = api.shutdown().await;
            if !http.is_finished() {
                (&mut http).await??;
            }
            cleanup
        })
        .await
        .context("API shutdown timed out");
        http.abort();
        let cleanup = cleanup?;
        result.and(cleanup)
    }
    .await;
    // Every worker gets its grace period at the same time, so a pool stops as
    // fast as a single worker.
    let cleanup =
        futures::future::join_all(workers.into_iter().flatten().map(runtime::Worker::shutdown))
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<()>>>()
            .map(drop);
    // Run both cleanups even when serving or initialization fails.
    if let Err(error) = &cleanup {
        tracing::error!(error = format!("{error:#}"), "worker cleanup failed");
    }
    if let Some(telemetry) = telemetry {
        telemetry.shutdown();
    }
    serving.and(cleanup)
}

/// The managed workers: one, or run mode's pool (see RUN_MODE.md).
struct Pool {
    executable: AbsolutePathBuf,
    /// Worker 0's home, which the operator configures.
    home: AbsolutePathBuf,
    size: usize,
}

impl Pool {
    /// Worker 0 uses the configured home, so a data directory keeps its
    /// threads when it moves from one worker to a pool; worker `i` uses
    /// `<home>-worker-<i>` beside it.
    fn home(&self, index: usize) -> anyhow::Result<AbsolutePathBuf> {
        if index == 0 {
            return Ok(self.home.clone());
        }
        let name = self
            .home
            .as_path()
            .file_name()
            .context("worker home has no name")?
            .to_string_lossy();
        Ok(AbsolutePathBuf::try_from(
            self.home
                .as_path()
                .with_file_name(format!("{name}-worker-{index}")),
        )?)
    }

    /// Start worker `index`. Every worker runs worker 0's configuration,
    /// copied into its own home before each start.
    async fn spawn(&self, index: usize) -> anyhow::Result<runtime::Worker> {
        let home = self.home(index)?;
        if index > 0 {
            match tokio::fs::read(self.home.join("config.toml")).await {
                Ok(config) => {
                    tokio::fs::create_dir_all(&home).await?;
                    tokio::fs::write(home.join("config.toml"), config).await?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("failed to read worker configuration"),
            }
        }
        runtime::Worker::spawn(self.executable.clone(), home).await
    }
}

/// Supervise one managed worker: whenever it exits, restart it and reattach
/// it to the API as the same worker. Returns only when restarts are exhausted.
async fn supervise(
    index: usize,
    slot: &mut Option<runtime::Worker>,
    api: &AgentsApi,
    pool: &Pool,
    startup_timeout: Duration,
) -> anyhow::Result<()> {
    loop {
        let exit = runtime::worker_exit(slot).await;
        tracing::warn!(
            worker = index,
            exit = exit.err().map(|error| format!("{error:#}")),
            "managed worker exited; attempting restart"
        );
        restart_worker(index, slot, api, pool, startup_timeout)
            .await
            .with_context(|| format!("managed worker {index}"))?;
    }
}

/// Bounded restarts prevent an unrecoverable worker from spinning forever;
/// exhaustion ends the process so the failure is visible to an operator.
const WORKER_RESTART_ATTEMPTS: usize = 5;
const WORKER_RESTART_BACKOFF: Duration = Duration::from_millis(/*millis*/ 200);
const WORKER_RESTART_BACKOFF_MAX: Duration = Duration::from_secs(/*secs*/ 5);

/// Reap the crashed worker, then respawn and reattach with bounded backoff.
///
/// The dead worker is dropped first so its home lock and socket are released
/// before a replacement claims them. Each replacement completes the app-server
/// initialization handshake (via `connect`) before `reconnect_worker` routes
/// requests to it.
async fn restart_worker(
    index: usize,
    worker: &mut Option<runtime::Worker>,
    api: &AgentsApi,
    pool: &Pool,
    startup_timeout: Duration,
) -> anyhow::Result<()> {
    if let Some(dead) = worker.take()
        && let Err(error) = dead.shutdown().await
    {
        tracing::warn!(
            worker = index,
            error = format!("{error:#}"),
            "reaping crashed worker failed"
        );
    }
    let mut backoff = WORKER_RESTART_BACKOFF;
    let mut last_error = None;
    for attempt in 1..=WORKER_RESTART_ATTEMPTS {
        tokio::time::sleep(backoff).await;
        match respawn(pool, index, startup_timeout).await {
            Ok((replacement, client)) => {
                let pid = replacement.id().context("worker process ID")?;
                // Owned before the reattach, so a shutdown meanwhile stops it
                // gracefully.
                *worker = Some(replacement);
                api.reconnect_worker(index, client).await?;
                tracing::info!(worker = index, pid, attempt, "managed worker restarted");
                exporters::count(exporters::WORKER_RESTART, &[("outcome", "restarted")]);
                return Ok(());
            }
            Err(error) => {
                tracing::warn!(
                    worker = index,
                    attempt,
                    error = format!("{error:#}"),
                    "worker restart attempt failed"
                );
                exporters::count(exporters::WORKER_RESTART, &[("outcome", "failed")]);
                last_error = Some(error);
                backoff = (backoff * 2).min(WORKER_RESTART_BACKOFF_MAX);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("worker restart produced no error")))
        .context("managed worker restart attempts exhausted")
}

/// Spawn one replacement worker and connect an initialized client to it,
/// failing fast if the fresh process exits before the handshake completes.
async fn respawn(
    pool: &Pool,
    index: usize,
    startup_timeout: Duration,
) -> anyhow::Result<(runtime::Worker, AppServerClient)> {
    let starting = Instant::now();
    let mut worker = Some(pool.spawn(index).await?);
    let socket = match &worker {
        Some(worker) => worker.socket().clone(),
        None => anyhow::bail!("worker missing after spawn"),
    };
    let client = tokio::select! {
        result = runtime::connect(socket, startup_timeout) => result?,
        result = runtime::worker_exit(&mut worker) => {
            return Err(result
                .err()
                .unwrap_or_else(|| anyhow::anyhow!("worker exited during restart"))
                .context("managed worker exited before initialization"));
        }
    };
    exporters::elapsed(
        exporters::WORKER_STARTUP_DURATION,
        starting.elapsed(),
        &[("reason", "restart")],
    );
    match worker {
        Some(worker) => Ok((worker, AppServerClient::Remote(client))),
        None => anyhow::bail!("worker vanished during initialization"),
    }
}

fn shutdown_signal() -> impl Future<Output = anyhow::Result<()>> {
    #[cfg(unix)]
    let termination = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
    #[cfg(unix)]
    let interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt());
    async move {
        #[cfg(unix)]
        {
            let mut termination = termination?;
            let mut interrupt = interrupt?;
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = termination.recv() => {},
            }
        }
        #[cfg(not(unix))]
        tokio::signal::ctrl_c().await?;
        Ok(())
    }
}
