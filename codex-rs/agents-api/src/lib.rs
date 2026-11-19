mod actions;
mod agent_tools;
mod artifacts;
mod capabilities;
mod configuration;
mod contract;
mod credentials;
mod egress;
mod environment_files;
mod environments;
mod files;
mod gates;
mod images;
mod input;
mod lifecycle;
pub mod managed;
mod mcp;
mod otlp;
pub mod projects;
mod reconcile;
mod records;
mod registry;
mod rendezvous;
mod resources;
mod routes;
mod secrets;
mod session_skills;
mod sessions;
mod skill_bundle;
mod skills;
mod stdio_env;
mod store;
mod streaming;
mod subagents;
mod telemetry;
mod traces;
mod turn_aliases;
mod turns;
mod upload;
mod usage;
mod vaults;
mod webhook_delivery;
mod webhook_outbox;
mod webhooks;

use anyhow::Context;
use axum::Json;
use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use codex_app_server_client::AppServerClient;
use codex_app_server_client::AppServerEvent;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerRequest;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use tokio::sync::Semaphore;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::watch;
use uuid::Uuid;

/// The live backend connection routing state. Exactly one pump task consumes
/// each connection, and pumps never overlap: a replacement is installed only
/// after the previous pump has exited and recorded its disconnect bookkeeping,
/// so pending function rows always belong to the connection that created them.
struct Backend {
    handle: AppServerRequestHandle,
    submissions: mpsc::Sender<actions::Submission>,
    cancel: watch::Sender<bool>,
    /// Threads started or resumed on this connection. A replacement connection
    /// starts empty, so each thread is resumed once per connection.
    loaded: Arc<Mutex<HashSet<String>>>,
    /// Self-hosted environments added to this connection's worker.
    environments: Arc<Mutex<HashSet<String>>>,
}

/// How long a request to the worker may take before the API gives up on it.
const RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// Worker calls at least this slow are logged.
const SLOW_RPC: std::time::Duration = std::time::Duration::from_secs(5);
/// The most workers one service runs (run mode's pool; see RUN_MODE.md).
pub const MAX_WORKERS: usize = 64;

struct State {
    store: store::Store,
    /// One connection slot per worker, `None` while that worker is down. A
    /// single worker serves every session unless run mode starts a pool.
    backends: Mutex<Vec<Option<Backend>>>,
    /// The worker each Codex thread lives on, as far as it has been looked up
    /// or started. A thread never moves between workers.
    thread_workers: Mutex<HashMap<String, usize>>,
    input_gates: gates::Gates,
    streams: streaming::Streams,
    /// Lower-case MCP hosts the operator allows to be internal or plain http.
    mcp_hosts: Mutex<HashSet<String>>,
    /// Lower-case webhook hosts the operator allows to be internal or plain http.
    webhook_hosts: Mutex<HashSet<String>>,
    image_hosts: Mutex<HashSet<String>>,
    /// Encrypted credential values, available once the operator supplies a passphrase.
    secrets: secrets::Secrets,
    /// Wakes the webhook dispatcher after a commit that may have queued deliveries.
    webhook_wake: tokio::sync::Notify,
    /// The environment registry and rendezvous state for self-hosted environments.
    registry: registry::Registry,
    /// The public origin advertised to executors instead of request headers.
    public_url: Mutex<Option<reqwest::Url>>,
    /// Input waiting for a self-hosted executor to connect.
    waits: environments::Waits,
    /// Provisions managed (`openai_hosted`) environments, when configured.
    provider: Mutex<Option<Arc<dyn managed::EnvironmentProvider>>>,
    /// Managed environments' snapshot, reset, suspension, and expiry timing.
    lifecycle: Mutex<managed::Lifecycle>,
    lifecycle_started: std::sync::atomic::AtomicBool,
    /// Wakes the lifecycle loop before its next tick, when a turn ends or a
    /// file is written, so the work is snapshotted promptly.
    lifecycle_wake: Arc<tokio::sync::Notify>,
    /// Managed environments a new sandbox is being provisioned for.
    reprovisioning: Mutex<HashSet<String>>,
    /// Sessions whose managed environment was replaced (resumed or reset):
    /// their stdio and environment-origin MCP servers ran in the old sandbox,
    /// so a thread still loaded reconnects them before its next turn.
    stale_mcp: Mutex<HashSet<String>>,
    /// How many consecutive worker-loss interruptions a session's turns are
    /// continued after (1 by default); 0 leaves interrupted sessions failed.
    continuations: std::sync::atomic::AtomicU32,
    /// Sessions a pass is deciding whether to continue.
    continuing: Mutex<HashSet<String>>,
    /// Public IDs of turns that were queued before Codex started them.
    turn_aliases: turn_aliases::TurnAliases,
    /// Each session's most recently started turn, as (thread ID, turn ID),
    /// for a cancel that arrives before Codex reports the turn.
    started_turns: Mutex<std::collections::HashMap<String, (String, String)>>,
    /// Turns cancelled before Codex made them active; interrupted when their
    /// start is recorded.
    pending_cancels: Mutex<HashSet<String>>,
    /// Turns a caller asked to cancel that have not finished, so a turn that
    /// history reports interrupted after a worker loss is known to be one the
    /// caller stopped rather than one the loss cut short.
    cancel_requested: Mutex<HashSet<String>>,
    /// Uploaded file contents.
    files: files::Files,
    /// Skill version contents.
    skills: skills::Skills,
    /// Skill contents pinned to self-hosted sessions, by session ID.
    skill_pins: std::path::PathBuf,
    /// This service's own connections to self-hosted executors, for
    /// environment files.
    executors: codex_exec_server::EnvironmentManager,
    events: broadcast::Sender<Value>,
    public_events: broadcast::Sender<Value>,
    token: String,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl State {
    fn provider(&self) -> Option<Arc<dyn managed::EnvironmentProvider>> {
        lock(&self.provider).clone()
    }

    /// How many workers serve this service: 1, or run mode's pool size.
    fn workers(&self) -> usize {
        lock(&self.backends).len()
    }

    fn backend<T>(&self, worker: usize, read: impl FnOnce(&Backend) -> T) -> Option<T> {
        lock(&self.backends)
            .get(worker)
            .and_then(Option::as_ref)
            .map(read)
    }

    /// Call the worker that owns the thread named by `params.threadId`, or,
    /// for a call that names no thread, the first connected worker: every
    /// worker of a pool runs the same configuration.
    async fn rpc(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        let worker = match params.get("threadId").and_then(Value::as_str) {
            Some(thread_id) => self.thread_worker(thread_id).await?,
            None => self.any_worker().ok_or_else(disconnected_error)?,
        };
        self.rpc_on(worker, method, params).await
    }

    async fn rpc_on(&self, worker: usize, method: &str, params: Value) -> Result<Value, ApiError> {
        // Clone the handle out of the lock; backend I/O must not hold it.
        let handle = self
            .backend(worker, |backend| backend.handle.clone())
            .ok_or_else(disconnected_error)?;
        let request = serde_json::from_value(
            json!({"id": Uuid::new_v4().to_string(), "method": method, "params": params}),
        )
        .map_err(anyhow::Error::from)?;
        // A transport failure means this connection is gone: report it as a
        // disconnection (503) rather than an internal error, so a request that
        // races a worker crash sees the documented recovery response. A
        // server-side JSON-RPC error is a genuine upstream failure (502).
        // A worker that never answers (a crashed request handler, a stuck
        // thread) must not hold the caller's request forever.
        let started = std::time::Instant::now();
        let answer = tokio::time::timeout(RPC_TIMEOUT, handle.request(request)).await;
        // Slow worker calls are logged, so a stall shows which calls queued
        // and for how long.
        let took = started.elapsed();
        if took >= SLOW_RPC {
            tracing::warn!(
                method,
                worker,
                took_ms = took.as_millis() as u64,
                answered = answer.is_ok(),
                "slow app-server request"
            );
        }
        answer
            .map_err(|_| {
                ApiError(
                    StatusCode::GATEWAY_TIMEOUT,
                    format!(
                        "app-server did not answer {method} within {} seconds",
                        RPC_TIMEOUT.as_secs()
                    ),
                )
            })?
            .map_err(|_| disconnected_error())?
            .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error.message))
    }

    /// Whether each worker is connected, by worker index.
    fn worker_states(&self) -> Vec<bool> {
        lock(&self.backends).iter().map(Option::is_some).collect()
    }

    fn any_worker(&self) -> Option<usize> {
        lock(&self.backends).iter().position(Option::is_some)
    }

    /// Whether the worker that serves this session is connected. A session
    /// not yet on a worker needs any connected worker to start on.
    async fn session_connected(&self, session_id: &str) -> Result<bool, ApiError> {
        Ok(match self.assigned_worker(session_id).await? {
            Some(worker) => self.backend(worker, |_| ()).is_some(),
            None => self.any_worker().is_some(),
        })
    }

    fn loaded_threads(&self, worker: usize) -> Result<Arc<Mutex<HashSet<String>>>, ApiError> {
        self.backend(worker, |backend| Arc::clone(&backend.loaded))
            .ok_or_else(disconnected_error)
    }

    fn attached_environments(
        &self,
        worker: usize,
    ) -> Result<Arc<Mutex<HashSet<String>>>, ApiError> {
        self.backend(worker, |backend| Arc::clone(&backend.environments))
            .ok_or_else(disconnected_error)
    }

    fn submissions(&self, worker: usize) -> Option<mpsc::Sender<actions::Submission>> {
        self.backend(worker, |backend| backend.submissions.clone())
    }

    /// The worker a session's threads live on: `None` for a session in a
    /// pool that has not started a thread yet. A session without an
    /// assignment that has a thread belongs to worker 0.
    async fn assigned_worker(&self, session_id: &str) -> Result<Option<usize>, ApiError> {
        if self.workers() == 1 {
            return Ok(Some(0));
        }
        let row: Option<(Option<i64>, Option<String>)> = sqlx::query_as(
            "SELECT (SELECT worker FROM session_workers WHERE session_id = ?1), \
             (SELECT thread_id FROM sessions WHERE id = ?1)",
        )
        .bind(session_id)
        .fetch_optional(&self.store.0)
        .await
        .map_err(anyhow::Error::from)?;
        Ok(match row {
            Some((Some(worker), _)) => Some(self.worker_index(worker)?),
            Some((None, Some(_))) => Some(0),
            _ => None,
        })
    }

    /// The worker a session's threads live on, assigning one first if the
    /// session has none: the connected worker with the fewest sessions, the
    /// lowest-numbered on a tie. One statement chooses and records the
    /// assignment, so sessions starting together cannot take the same free
    /// worker.
    async fn session_worker(&self, session_id: &str) -> Result<usize, ApiError> {
        if let Some(worker) = self.assigned_worker(session_id).await? {
            return Ok(worker);
        }
        let states = self.worker_states();
        let mut candidates: Vec<usize> = (0..states.len()).filter(|&i| states[i]).collect();
        if candidates.is_empty() {
            candidates = (0..states.len()).collect();
        }
        sqlx::query(
            "INSERT OR IGNORE INTO session_workers (session_id, worker) \
             SELECT ?, c.value FROM json_each(?) c WHERE true \
             ORDER BY (SELECT count(*) FROM session_workers w JOIN sessions s ON s.id = w.session_id \
                       WHERE w.worker = c.value), c.value LIMIT 1",
        )
        .bind(session_id)
        .bind(json!(candidates).to_string())
        .execute(&self.store.0)
        .await
        .map_err(anyhow::Error::from)?;
        let worker: i64 =
            sqlx::query_scalar("SELECT worker FROM session_workers WHERE session_id = ?")
                .bind(session_id)
                .fetch_one(&self.store.0)
                .await
                .map_err(anyhow::Error::from)?;
        let worker = self.worker_index(worker)?;
        tracing::info!(session_id, worker, "session assigned to worker");
        Ok(worker)
    }

    /// The worker a Codex thread lives on: its session's (a subagent's thread
    /// is its session's), or, for a deleted session's thread, the one
    /// recorded for its deletion. An unknown thread belongs to worker 0.
    async fn thread_worker(&self, thread_id: &str) -> Result<usize, ApiError> {
        if self.workers() == 1 {
            return Ok(0);
        }
        if let Some(&worker) = lock(&self.thread_workers).get(thread_id) {
            return Ok(worker);
        }
        let worker: Option<i64> = sqlx::query_scalar(
            "SELECT worker FROM ( \
               SELECT COALESCE((SELECT worker FROM session_workers WHERE session_id = s.id), 0) AS worker, 0 AS rank \
                 FROM sessions s WHERE s.thread_id = ?1 \
               UNION ALL SELECT COALESCE((SELECT worker FROM session_workers WHERE session_id = a.session_id), 0), 1 \
                 FROM subagents a WHERE a.thread_id = ?1 \
               UNION ALL SELECT worker, 2 FROM session_cleanup WHERE thread_id = ?1) \
             ORDER BY rank LIMIT 1",
        )
        .bind(thread_id)
        .fetch_optional(&self.store.0)
        .await
        .map_err(anyhow::Error::from)?;
        let Some(worker) = worker else {
            return Ok(0);
        };
        let worker = self.worker_index(worker)?;
        self.remember_thread(thread_id, worker);
        Ok(worker)
    }

    fn remember_thread(&self, thread_id: &str, worker: usize) {
        if self.workers() > 1 {
            lock(&self.thread_workers).insert(thread_id.to_owned(), worker);
        }
    }

    /// A stored worker index, checked against the pool: a session assigned
    /// by a larger pool than this one runs cannot be served.
    fn worker_index(&self, worker: i64) -> Result<usize, ApiError> {
        let workers = self.workers();
        usize::try_from(worker)
            .ok()
            .filter(|&worker| worker < workers)
            .ok_or_else(|| {
                ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!(
                        "the session's worker {worker} is not running; this service runs {workers}"
                    ),
                )
            })
    }

    /// The sessions served by `worker`, which a loss of that worker affects.
    async fn worker_sessions(&self, worker: usize) -> anyhow::Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT id FROM sessions s \
             WHERE COALESCE((SELECT worker FROM session_workers w WHERE w.session_id = s.id), 0) = ?",
        )
        .bind(worker as i64)
        .fetch_all(&self.store.0)
        .await?)
    }
}

fn disconnected_error() -> ApiError {
    ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "app-server disconnected".into(),
    )
}

struct ApiError(StatusCode, String);

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        tracing::error!(error = format!("{error:#}"), "internal error");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal API error".into(),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

impl ApiError {
    fn public_response(self) -> axum::response::Response {
        let kind = if self.0.is_server_error() {
            "server_error"
        } else {
            "invalid_request_error"
        };
        // The pinned SDK recognizes the pending-call registration race by code.
        let code = self
            .1
            .starts_with(input::UNKNOWN_PENDING_CALL)
            .then_some("invalid_request_error");
        (
            self.0,
            Json(json!({"error": {"message":self.1,"type":kind,"param":null,"code":code}})),
        )
            .into_response()
    }
}

/// The JSON body limit for requests that carry agent configuration or input:
/// agent instructions and message text of up to 1 MiB each, with room for
/// tools and data-URL images. Other JSON requests are limited to 16 KiB.
pub(crate) const LARGE_JSON_BODY: usize = 8 << 20;

/// HTTP facade over dedicated Codex app-server connections: one worker, or
/// run mode's pool with each session on one of them.
/// Dropping this owner requests shutdown; call `shutdown` to wait for completion.
pub struct AgentsApi {
    state: Arc<State>,
    directory: AbsolutePathBuf,
    router: Router,
    /// Each worker's notification pump, by worker index.
    pumps: Mutex<Vec<Option<tokio::task::JoinHandle<std::io::Result<()>>>>>,
    dispatcher: tokio::task::JoinHandle<()>,
    /// Serializes replacements of each worker's connection.
    reconnect_gates: Vec<Semaphore>,
    stop: watch::Sender<bool>,
}

impl AgentsApi {
    pub async fn new(
        client: AppServerClient,
        directory: AbsolutePathBuf,
        token: String,
    ) -> anyhow::Result<Self> {
        Self::with_workers(vec![client], directory, token).await
    }

    /// Serve sessions on a pool of workers, one connection each (run mode;
    /// see RUN_MODE.md). Each session is assigned to one worker on its first
    /// input and stays there; worker `i` is replaced with
    /// [`Self::reconnect_worker`]. A pool of one is [`Self::new`].
    pub async fn with_workers(
        clients: Vec<AppServerClient>,
        directory: AbsolutePathBuf,
        token: String,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            token.len() >= 32,
            "API token must contain at least 32 bytes"
        );
        anyhow::ensure!(
            (1..=MAX_WORKERS).contains(&clients.len()),
            "a service runs from 1 to {MAX_WORKERS} workers"
        );
        let workers = clients.len();
        let store = store::Store::open(directory.clone()).await?;
        let turn_aliases = turn_aliases::TurnAliases::load(&store.0).await?;
        let state = Arc::new(State {
            store,
            turn_aliases,
            backends: Mutex::new((0..workers).map(|_| None).collect()),
            thread_workers: Mutex::default(),
            input_gates: gates::Gates::default(),
            streams: streaming::Streams::default(),
            mcp_hosts: Mutex::default(),
            webhook_hosts: Mutex::default(),
            image_hosts: Mutex::default(),
            secrets: secrets::Secrets::default(),
            webhook_wake: tokio::sync::Notify::new(),
            registry: registry::Registry::default(),
            public_url: Mutex::default(),
            waits: environments::Waits::default(),
            provider: Mutex::default(),
            lifecycle: Mutex::default(),
            lifecycle_started: std::sync::atomic::AtomicBool::new(false),
            lifecycle_wake: Arc::default(),
            reprovisioning: Mutex::default(),
            stale_mcp: Mutex::default(),
            continuations: std::sync::atomic::AtomicU32::new(1),
            continuing: Mutex::default(),
            started_turns: Mutex::default(),
            pending_cancels: Mutex::default(),
            cancel_requested: Mutex::default(),
            files: files::Files(directory.join("files").to_path_buf()),
            skills: skills::Skills(directory.join("skills").to_path_buf()),
            skill_pins: directory.join("skill_pins").to_path_buf(),
            executors: codex_exec_server::EnvironmentManager::without_environments(
                codex_http_client::HttpClientFactory::new(
                    codex_http_client::OutboundProxyPolicy::ReqwestDefault,
                ),
            ),
            events: broadcast::channel(/*capacity*/ 128).0,
            // Text deltas arrive at token rate; a consumer that falls this far
            // behind is closed and recovers from saved records.
            public_events: broadcast::channel(/*capacity*/ 1024).0,
            token,
        });
        environments::recover(&state).await?;
        files::sweep(&state).await?;
        skills::sweep(&state).await?;
        session_skills::sweep(&state).await?;
        let router = routes::router(Arc::clone(&state));
        let stop = watch::channel(/*stopping*/ false).0;
        let dispatcher = tokio::spawn(webhook_outbox::dispatch(
            Arc::clone(&state),
            stop.subscribe(),
        ));
        let api = Self {
            state,
            directory,
            router,
            pumps: Mutex::new((0..workers).map(|_| None).collect()),
            dispatcher,
            reconnect_gates: (0..workers)
                .map(|_| Semaphore::new(/*permits*/ 1))
                .collect(),
            stop,
        };
        // Every worker serves before recovery runs, so it reaches every
        // session's thread at once.
        for (worker, client) in clients.into_iter().enumerate() {
            api.attach(worker, client).await?;
        }
        api.recover(/*replacing*/ false).await;
        Ok(api)
    }

    /// Replace the backend connection (worker 0's, in a pool).
    ///
    /// The previous connection is retired first: its pump exits, marks its
    /// unresolvable function calls `unavailable`, and shuts its own client
    /// down before the replacement starts, so stale callbacks cannot resolve
    /// work belonging to the new connection. Durable retrieval keeps serving
    /// throughout. A caller-owned worker process behind the old connection is
    /// never terminated by this API.
    pub async fn reconnect(&self, client: AppServerClient) -> anyhow::Result<()> {
        self.reconnect_worker(/*worker*/ 0, client).await
    }

    /// Replace worker `worker`'s connection, as [`Self::reconnect`] does for a
    /// single worker. Only that worker's sessions see the loss; the other
    /// workers keep serving theirs.
    pub async fn reconnect_worker(
        &self,
        worker: usize,
        client: AppServerClient,
    ) -> anyhow::Result<()> {
        let gate = self
            .reconnect_gates
            .get(worker)
            .with_context(|| format!("no worker {worker}"))?;
        let _replacing = gate.acquire().await.map_err(anyhow::Error::from)?;
        let replacing = self.attach(worker, client).await?;
        self.recover(replacing).await;
        Ok(())
    }

    /// The number of workers this service serves sessions on.
    pub fn workers(&self) -> usize {
        self.state.workers()
    }

    /// Retire worker `worker`'s connection, if any, and start serving
    /// `client` in its place. Returns whether a connection was replaced.
    async fn attach(&self, worker: usize, client: AppServerClient) -> anyhow::Result<bool> {
        let previous = lock(&self.state.backends)
            .get_mut(worker)
            .with_context(|| format!("no worker {worker}"))?
            .take();
        if let Some(previous) = previous {
            let _ = previous.cancel.send(true);
        }
        let retired = lock(&self.pumps)[worker].take();
        let replacing = retired.is_some();
        if let Some(retired) = retired {
            retired.await??;
        }
        let handle = client.request_handle();
        let (submissions, submissions_rx) = mpsc::channel(/*buffer*/ 32);
        let (cancel, cancelled) = watch::channel(/*cancelled*/ false);
        let identity = submissions.clone();
        lock(&self.state.backends)[worker] = Some(Backend {
            handle,
            submissions,
            cancel,
            loaded: Arc::default(),
            environments: Arc::default(),
        });
        let task = tokio::spawn(pump(
            Arc::clone(&self.state),
            worker,
            client,
            self.stop.subscribe(),
            cancelled,
            submissions_rx,
            identity,
        ));
        lock(&self.pumps)[worker] = Some(task);
        tracing::info!(worker, "backend connected");
        telemetry::count(telemetry::BACKEND_CONNECTION, &[("event", "connected")]);
        Ok(replacing)
    }

    /// Recovery once a connection serves.
    async fn recover(&self, replacing: bool) {
        // Correct any turns a prior disconnect failed provisionally against the
        // now-reachable rollout history. Best-effort: a failure here must not
        // fail the reconnect, since the service is otherwise ready.
        if let Err(error) = reconcile::run(&self.state).await {
            tracing::warn!(error = format!("{error:#}"), "reconciliation failed");
        }
        // Retry thread deletions a previous connection or process left queued.
        if let Err(error) = sessions::cleanup(&self.state).await {
            tracing::warn!(error = format!("{error:#}"), "session cleanup failed");
        }
        // A replacement worker continues the turns its predecessor's loss cut
        // short. At startup that waits for the operator's setting
        // ([`Self::set_interrupted_turn_continuations`]).
        if replacing {
            reconcile::continue_interrupted(&self.state);
        }
    }

    pub fn router(&self) -> Router {
        self.router.clone()
    }

    /// Enable vault credentials, encrypting their values in the data directory
    /// under this passphrase. The same passphrase must be supplied on every
    /// start; one that cannot read the credentials stored earlier is refused.
    pub async fn configure_vault(&self, passphrase: String) -> anyhow::Result<()> {
        anyhow::ensure!(
            passphrase.len() >= 32,
            "vault passphrase must contain at least 32 bytes"
        );
        let stored = self
            .state
            .secrets
            .configure(
                self.directory.to_path_buf(),
                self.state.store.0.clone(),
                passphrase,
            )
            .await?;
        let removed = credentials::sweep(&self.state, stored).await?;
        if removed > 0 {
            tracing::info!(removed, "removed secrets left by interrupted deletions");
        }
        Ok(())
    }

    /// Advertise this http(s) origin in executor-facing registry and rendezvous
    /// URLs instead of using the request's Host and x-forwarded-proto headers.
    /// The worker's harness rendezvous URL always follows its request headers.
    pub fn configure_public_url(&self, public_url: String) -> anyhow::Result<()> {
        *lock(&self.state.public_url) = Some(registry::parse_public_url(&public_url)?);
        Ok(())
    }

    /// Accept self-hosted executors that authenticate with this environment
    /// key. It grants only the registry endpoints, never the API. The worker
    /// reaches the registry at `registry_url`, this service's own address
    /// followed by `/registry`.
    pub fn configure_environments(
        &self,
        environment_key: String,
        registry_url: String,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            environment_key.len() >= 32,
            "environment key must contain at least 32 bytes"
        );
        anyhow::ensure!(
            environment_key != self.state.token,
            "environment key must differ from the API token"
        );
        self.state.registry.configure(environment_key, registry_url);
        Ok(())
    }

    /// Provision `openai_hosted` environments with `provider`. They also need
    /// the environment registry ([`Self::configure_environments`]), which the
    /// provisioned executors dial; set a public URL they can reach.
    pub fn configure_environment_provider(&self, provider: Arc<dyn managed::EnvironmentProvider>) {
        *lock(&self.state.provider) = Some(provider);
        lifecycle::spawn(&self.state);
    }

    /// Set managed environments' snapshot, reset, suspension, and expiry
    /// timing; [`managed::Lifecycle::default`] applies otherwise.
    pub fn set_managed_lifecycle(&self, lifecycle: managed::Lifecycle) {
        *lock(&self.state.lifecycle) = lifecycle;
    }

    /// Continue a session's turn that a worker loss interrupted, by sending it
    /// [`reconcile::CONTINUE_INPUT`] once a worker serves again, for up to
    /// `attempts` consecutive interruptions (1 by default); 0 leaves such
    /// sessions failed. Call it at startup: sessions a previous process left
    /// interrupted are continued (or failed) then.
    pub fn set_interrupted_turn_continuations(&self, attempts: u32) {
        self.state
            .continuations
            .store(attempts, std::sync::atomic::Ordering::Relaxed);
        reconcile::continue_interrupted(&self.state);
    }

    /// How long input waits for a self-hosted executor to connect before it is
    /// dropped. The contract's five minutes is the default.
    pub fn set_environment_connection_wait(&self, wait: std::time::Duration) {
        *lock(&self.state.waits.limit) = wait;
    }

    /// How often a connected executor's key is re-checked, which bounds how
    /// long a revoked key keeps an open executor socket. Thirty seconds is the
    /// default.
    pub fn set_executor_reauthorize_interval(&self, interval: std::time::Duration) {
        *lock(&self.state.registry.reauthorize_interval) = interval;
    }

    /// Allow public MCP servers on these hosts even when they resolve to
    /// loopback, private, or link-local addresses, or use plain http.
    pub fn allow_mcp_hosts(&self, hosts: impl IntoIterator<Item = String>) {
        lock(&self.state.mcp_hosts).extend(hosts.into_iter().map(|host| host.to_ascii_lowercase()));
    }

    /// Allow webhook deliveries to these hosts even when they resolve to
    /// loopback, private, or link-local addresses, or use plain http.
    pub fn allow_webhook_hosts(&self, hosts: impl IntoIterator<Item = String>) {
        lock(&self.state.webhook_hosts)
            .extend(hosts.into_iter().map(|host| host.to_ascii_lowercase()));
    }

    /// Allow input images from these hosts even when they resolve to
    /// loopback, private, or link-local addresses, or use plain http.
    pub fn allow_image_hosts(&self, hosts: impl IntoIterator<Item = String>) {
        lock(&self.state.image_hosts)
            .extend(hosts.into_iter().map(|host| host.to_ascii_lowercase()));
    }

    /// Close the backend connection, allowing Codex to flush session state.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        let _ = self.stop.send(true);
        // An interrupted delivery stays queued and is repeated after restart.
        self.dispatcher.abort();
        let pumps: Vec<_> = lock(&self.pumps)
            .iter_mut()
            .filter_map(Option::take)
            .collect();
        // Every connection closes at once, so a pool stops as fast as one.
        for result in futures::future::join_all(pumps).await {
            result??;
        }
        Ok(())
    }
}

impl Drop for AgentsApi {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

/// Consumes one backend connection's notifications until it disconnects, the
/// API stops, or a replacement retires it. On exit it stops routing mutations
/// to this connection, then records the disconnect exactly once; `reconnect`
/// awaits that completion before a new connection may create function rows.
async fn pump(
    state: Arc<State>,
    worker: usize,
    mut client: AppServerClient,
    mut stopping: watch::Receiver<bool>,
    mut cancelled: watch::Receiver<bool>,
    mut submissions: mpsc::Receiver<actions::Submission>,
    identity: mpsc::Sender<actions::Submission>,
) -> std::io::Result<()> {
    let mut lost = false;
    loop {
        let event = tokio::select! {
            biased;
            event = client.next_event() => event,
            _ = stopping.changed() => break,
            _ = cancelled.changed() => break,
            Some(submission) = submissions.recv() => {
                actions::resolve(&state, &client, submission).await;
                continue;
            }
        };
        let Some(event) = event else {
            lost = true;
            break;
        };
        match event {
            AppServerEvent::ServerNotification(notification) => {
                if let Ok(mut value) = serde_json::to_value(notification) {
                    if state
                        .turn_aliases
                        .translate(&state.store.0, &mut value)
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if actions::notification(&state, &value).await.is_err() {
                        break;
                    }
                    if records::notification(&state, &value).await.is_err() {
                        break;
                    }
                    if let Err(error) = streaming::notification(&state, &value).await {
                        tracing::warn!(error = format!("{error:#}"), "stream event dropped");
                    }
                    let _ = state.events.send(value);
                }
            }
            AppServerEvent::ServerRequest(request) => {
                let request_id = request.id().clone();
                if let ServerRequest::DynamicToolCall { mut params, .. } = *request
                    && {
                        params.turn_id = state.turn_aliases.public(&params.turn_id);
                        actions::register(&state, &request_id, params).await.is_ok()
                    }
                {
                    continue;
                }
                let _ = client
                    .reject_server_request(
                        request_id,
                        JSONRPCErrorError {
                            code: -32601,
                            message: "unsupported or invalid interactive request".into(),
                            data: None,
                        },
                    )
                    .await;
            }
            AppServerEvent::Lagged { skipped } => {
                // Missing backend notifications leave the public projection incomplete.
                lost = true;
                let _ = state
                    .events
                    .send(json!({"method": "stream/lagged", "params": {"skipped": skipped}}));
                break;
            }
            AppServerEvent::Disconnected { .. } => {
                lost = true;
                break;
            }
        }
    }
    // Stop routing mutations to this connection before recording the loss.
    // A replacement may already have taken the slot; only clear our own entry.
    {
        let mut backends = lock(&state.backends);
        if let Some(slot) = backends.get_mut(worker)
            && slot
                .as_ref()
                .is_some_and(|backend| backend.submissions.same_channel(&identity))
        {
            *slot = None;
        }
    }
    if lost {
        tracing::warn!(worker, "backend connection lost");
        telemetry::count(telemetry::BACKEND_CONNECTION, &[("event", "lost")]);
    }
    if let Err(error) = disconnect_bookkeeping(&state, worker).await {
        tracing::error!(
            error = format!("{error:#}"),
            "disconnect bookkeeping failed"
        );
    }
    match client.shutdown().await {
        Err(error) if lost => {
            tracing::warn!(%error, "backend cleanup after connection loss failed");
            Ok(())
        }
        result => result,
    }
}

/// The lost connection's JSON-RPC waiters cannot be recreated, so its pending
/// function calls become unavailable and its interrupted public work is marked
/// failed. Runs once per retired connection, before any replacement serves.
/// In a pool, only the lost worker's sessions are touched.
async fn disconnect_bookkeeping(state: &State, worker: usize) -> anyhow::Result<()> {
    let sessions = if state.workers() == 1 {
        None
    } else {
        Some(state.worker_sessions(worker).await?)
    };
    let scope = sessions
        .as_ref()
        .map(|sessions| json!(sessions).to_string());
    sqlx::query(
        "UPDATE tool_calls SET status = 'unavailable' WHERE status IN ('pending', 'submitting') \
         AND (?1 IS NULL OR session_id IN (SELECT value FROM json_each(?1)))",
    )
    .bind(&scope)
    .execute(&state.store.0)
    .await?;
    let continuing = state
        .continuations
        .load(std::sync::atomic::Ordering::Relaxed)
        > 0;
    records::disconnected(&state.store.0, continuing, scope.as_deref()).await?;
    match &sessions {
        None => {
            state.streams.clear();
            let _ = state.events.send(json!({"method": "stream/disconnected"}));
            let _ = state.public_events.send(json!({"type":"disconnect"}));
        }
        Some(sessions) => {
            state.streams.clear_sessions(sessions);
            let _ = state
                .events
                .send(json!({"method": "stream/disconnected", "params": {"worker": worker}}));
            // Only the lost worker's sessions' streams end.
            let _ = state
                .public_events
                .send(json!({"type":"disconnect", "worker": worker, "session_ids": sessions}));
        }
    }
    Ok(())
}
