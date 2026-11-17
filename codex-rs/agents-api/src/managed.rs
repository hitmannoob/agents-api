//! Managed environments: an `openai_hosted` session gets an executor that the
//! service provisions through an [`EnvironmentProvider`], instead of one the
//! caller runs.
//!
//! The provider starts a sandbox running the stock `codex exec-server`, which
//! dials this service's registry with a connect token that only this
//! environment accepts. From then on the environment is a self-hosted one:
//! commands, files, skills, executor loss, and reconnection all take the
//! self-hosted path. The service reports `pending` while the sandbox starts,
//! `ready` once it runs, and `connected` when its executor dials in, and
//! destroys the sandbox when the session is deleted.
//!
//! The confidential `env` and `setup_commands` exist only for the provisioning
//! call and are never stored or returned. Provisioning that a restart
//! interrupts fails the environment instead of running again without them.
use crate::ApiError;
use crate::State;
use crate::contract::invalid;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;
use uuid::Uuid;

/// Where a managed environment's commands run, unless its provider says otherwise.
pub const WORKSPACE: &str = "/workspace";
const SIZES: [&str; 3] = ["small", "medium", "large"];
const MAX_PACKAGES: usize = 256;
const MAX_NAME_BYTES: usize = 256;
const MAX_ENV: usize = 128;
const MAX_ENV_BYTES: usize = 64 * 1024;
const MAX_SETUP_COMMANDS: usize = 32;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
/// How long one provider call may take; a first image build is the slow case.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(45 * 60);

/// Packages installed before the agent starts.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Packages {
    #[serde(default)]
    pub npm: Vec<String>,
    #[serde(default)]
    pub python: Vec<String>,
    #[serde(default)]
    pub system: Vec<String>,
}

/// A command run in the sandbox before the executor starts.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetupCommand {
    pub command: String,
    pub cwd: Option<String>,
}

/// What a provider needs to start one environment's executor. It holds the
/// connect token and confidential values, so it is never logged.
#[derive(Clone, Serialize)]
pub struct ProvisionRequest {
    pub environment_id: String,
    /// The registry the executor dials (`codex exec-server --remote`).
    pub registry_url: String,
    /// The executor's `CODEX_API_KEY`, accepted for this environment only.
    pub token: String,
    /// `small`, `medium`, or `large`.
    pub container_size: String,
    pub packages: Packages,
    pub env: BTreeMap<String, String>,
    pub setup_commands: Vec<SetupCommand>,
}

/// Starts and removes the sandboxes managed environments run in.
pub trait EnvironmentProvider: Send + Sync {
    /// The directory the executor's commands run in.
    fn workspace(&self) -> String {
        WORKSPACE.to_owned()
    }

    /// Start a sandbox whose executor dials the registry; returns its ID.
    fn provision(&self, request: ProvisionRequest) -> BoxFuture<'static, anyhow::Result<String>>;

    /// Stop and remove a sandbox.
    fn destroy(&self, sandbox_id: String) -> BoxFuture<'static, anyhow::Result<()>>;
}

/// A provider in another process that answers JSON lines on stdin and stdout,
/// such as `deploy/modal/provider.py`. Each request carries an `id` its answer
/// echoes, so calls run concurrently. The process starts on the first call and
/// again after it exits; calls waiting on a process that exits fail.
pub struct SidecarProvider(Arc<Sidecar>);

/// A call waiting for its answer, and the provider process it was sent to.
type Waiter = (u64, oneshot::Sender<Result<Value, String>>);

struct Sidecar {
    program: String,
    args: Vec<String>,
    /// The running process's generation and its stdin's line queue.
    lines: Mutex<Option<(u64, tokio::sync::mpsc::UnboundedSender<String>)>>,
    generations: AtomicU64,
    next: AtomicU64,
    pending: Mutex<HashMap<u64, Waiter>>,
}

impl SidecarProvider {
    pub fn new(program: String, args: Vec<String>) -> Self {
        Self(Arc::new(Sidecar {
            program,
            args,
            lines: Mutex::default(),
            generations: AtomicU64::new(0),
            next: AtomicU64::new(1),
            pending: Mutex::default(),
        }))
    }
}

impl Sidecar {
    async fn call(self: &Arc<Self>, mut request: Value) -> anyhow::Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        request["id"] = json!(id);
        let (sender, receiver) = oneshot::channel();
        self.send(id, sender, format!("{request}\n"))?;
        let answer = tokio::time::timeout(PROVIDER_TIMEOUT, receiver).await;
        crate::lock(&self.pending).remove(&id);
        match answer {
            Ok(Ok(Ok(answer))) => Ok(answer),
            Ok(Ok(Err(error))) => Err(anyhow::anyhow!(error)),
            Ok(Err(_)) => anyhow::bail!("the environment provider exited"),
            Err(_) => anyhow::bail!("the environment provider did not answer in time"),
        }
    }

    /// Queue a request line for the running process, starting one if none runs.
    fn send(
        self: &Arc<Self>,
        id: u64,
        waiter: oneshot::Sender<Result<Value, String>>,
        line: String,
    ) -> anyhow::Result<()> {
        let mut lines = crate::lock(&self.lines);
        if let Some((generation, sender)) = lines.as_ref()
            && !sender.is_closed()
        {
            crate::lock(&self.pending).insert(id, (*generation, waiter));
            sender.send(line)?;
            return Ok(());
        }
        let mut child = tokio::process::Command::new(&self.program)
            .args(&self.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdout"))?;
        let generation = self.generations.fetch_add(1, Ordering::Relaxed) + 1;
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        crate::lock(&self.pending).insert(id, (generation, waiter));
        sender.send(line)?;
        *lines = Some((generation, sender));
        drop(lines);
        tokio::spawn(write(child, stdin, receiver));
        tokio::spawn(Arc::clone(self).read(generation, stdout));
        Ok(())
    }

    /// Route answers to their callers; when the process exits, fail the calls
    /// still waiting on it.
    async fn read(self: Arc<Self>, generation: u64, stdout: tokio::process::ChildStdout) {
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(answer) = serde_json::from_str::<Value>(&line) else {
                tracing::warn!("environment provider wrote a line that is not JSON");
                continue;
            };
            let Some((_, waiter)) = answer["id"]
                .as_u64()
                .and_then(|id| crate::lock(&self.pending).remove(&id))
            else {
                continue;
            };
            let result = if answer["ok"] == true {
                Ok(answer)
            } else {
                Err(answer["error"]
                    .as_str()
                    .unwrap_or("the environment provider failed")
                    .to_owned())
            };
            let _ = waiter.send(result);
        }
        let mut running = crate::lock(&self.lines);
        if running
            .as_ref()
            .is_some_and(|(current, _)| *current == generation)
        {
            *running = None;
        }
        drop(running);
        // Dropping a waiter fails its call as "the environment provider exited".
        crate::lock(&self.pending).retain(|_, (sent_to, _)| *sent_to != generation);
    }
}

/// Write request lines to the provider until it exits or its queue closes.
/// Owning the child keeps it alive, and dropping it kills it.
async fn write(
    _child: tokio::process::Child,
    mut stdin: tokio::process::ChildStdin,
    mut lines: tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    while let Some(line) = lines.recv().await {
        if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
            break;
        }
    }
}

impl EnvironmentProvider for SidecarProvider {
    fn provision(&self, request: ProvisionRequest) -> BoxFuture<'static, anyhow::Result<String>> {
        let sidecar = Arc::clone(&self.0);
        Box::pin(async move {
            let mut message = serde_json::to_value(&request)?;
            message["op"] = json!("provision");
            let answer = sidecar.call(message).await?;
            answer["sandbox_id"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("the environment provider returned no sandbox"))
        })
    }

    fn destroy(&self, sandbox_id: String) -> BoxFuture<'static, anyhow::Result<()>> {
        let sidecar = Arc::clone(&self.0);
        Box::pin(async move {
            sidecar
                .call(json!({"op": "destroy", "sandbox_id": sandbox_id}))
                .await
                .map(|_| ())
        })
    }
}

/// The parts of an `openai_hosted` request this service supports.
pub(crate) struct Hosted {
    container_size: String,
    packages: Packages,
    env: BTreeMap<String, String>,
    setup_commands: Vec<SetupCommand>,
}

fn empty(value: &Value) -> bool {
    value.is_null() || value.as_array().is_some_and(Vec::is_empty)
}

/// A package name the provider can pass to its installer as one argument.
fn package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && !name.starts_with('-')
        && !name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "'\"`$\\;&|".contains(c))
}

fn env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name != "CODEX_API_KEY"
        && !name.starts_with("AGENTS_API_")
}

/// Parse an `openai_hosted` environment. Fields this service does not support
/// yet are refused rather than ignored.
pub(crate) fn parse(value: &Value) -> Result<Hosted, ApiError> {
    let fields = value
        .as_object()
        .ok_or_else(|| invalid("environment must be an object"))?;
    for (key, field) in fields {
        let supported = match key.as_str() {
            "type" | "container_size" | "packages" | "env" | "setup_commands" => true,
            "capability_directories" | "files" | "skills" | "plugins" => empty(field),
            "desktop" => field.is_null() || field == &json!({"enabled": false}),
            "network" => {
                field.is_null()
                    || (field["access"] == "enabled"
                        && field.as_object().is_some_and(|network| {
                            network
                                .iter()
                                .all(|(name, value)| name == "access" || empty(value))
                        }))
            }
            _ => false,
        };
        if !supported {
            return Err(invalid(format!(
                "openai_hosted environment field {key} is not supported by this service yet"
            )));
        }
    }
    let container_size = match &fields.get("container_size") {
        None | Some(Value::Null) => "medium".to_owned(),
        Some(Value::String(size)) if SIZES.contains(&size.as_str()) => size.clone(),
        Some(_) => return Err(invalid("container_size must be small, medium, or large")),
    };
    let packages: Packages = match fields.get("packages") {
        None | Some(Value::Null) => Packages::default(),
        Some(packages) => serde_json::from_value(packages.clone())
            .map_err(|_| invalid("packages takes npm, python, and system lists of names"))?,
    };
    let names = [&packages.npm, &packages.python, &packages.system];
    if names.iter().map(|list| list.len()).sum::<usize>() > MAX_PACKAGES
        || !names
            .iter()
            .flat_map(|list| list.iter())
            .all(|name| package_name(name))
    {
        return Err(invalid(format!(
            "packages must name at most {MAX_PACKAGES} packages, each one argument of at most {MAX_NAME_BYTES} bytes"
        )));
    }
    let env: BTreeMap<String, String> = match fields.get("env") {
        None | Some(Value::Null) => BTreeMap::new(),
        Some(env) => serde_json::from_value(env.clone())
            .map_err(|_| invalid("env must map names to string values"))?,
    };
    if env.len() > MAX_ENV
        || env
            .iter()
            .map(|(name, value)| name.len() + value.len())
            .sum::<usize>()
            > MAX_ENV_BYTES
        || !env.keys().all(|name| env_name(name))
    {
        return Err(invalid(format!(
            "env takes at most {MAX_ENV} variables and {MAX_ENV_BYTES} bytes, named like shell variables (CODEX_API_KEY and AGENTS_API_* are reserved)"
        )));
    }
    let setup_commands: Vec<SetupCommand> = match fields.get("setup_commands") {
        None | Some(Value::Null) => Vec::new(),
        Some(commands) => serde_json::from_value(commands.clone())
            .map_err(|_| invalid("setup_commands must be a list of {command, cwd}"))?,
    };
    if setup_commands.len() > MAX_SETUP_COMMANDS
        || setup_commands.iter().any(|step| {
            step.command.trim().is_empty()
                || step.command.len() > MAX_COMMAND_BYTES
                || step
                    .cwd
                    .as_deref()
                    .is_some_and(|cwd| !cwd.starts_with('/') || cwd.contains('\0'))
        })
    {
        return Err(invalid(format!(
            "setup_commands takes at most {MAX_SETUP_COMMANDS} commands of at most {MAX_COMMAND_BYTES} bytes, with absolute cwd paths"
        )));
    }
    Ok(Hosted {
        container_size,
        packages,
        env,
        setup_commands,
    })
}

pub(crate) fn digest(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

/// The environment's connect-token digest, when it is a managed environment.
pub(crate) async fn token_digest(
    state: &State,
    environment_id: &str,
) -> anyhow::Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT token_sha256 FROM environments WHERE id = ?")
            .bind(environment_id)
            .fetch_optional(&state.store.0)
            .await?
            .flatten(),
    )
}

/// Record a managed environment and return its public object with the
/// provisioning request to start once the session exists.
pub(crate) async fn create(
    state: &State,
    session_id: &str,
    environment_id: &str,
    hosted: Hosted,
    registry_url: String,
) -> anyhow::Result<(Value, ProvisionRequest)> {
    let public = json!({"id": environment_id, "type": "openai_hosted", "capability_directories": [], "files": [],
        "network": {"access": "enabled", "allowed_domains": []}, "packages": hosted.packages, "plugins": [],
        "skills": [], "container_size": hosted.container_size, "desktop": null});
    let token = format!(
        "envtok_{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    );
    sqlx::query("INSERT INTO environments (id, session_id, data, provider, token_sha256) VALUES (?, ?, ?, 'managed', ?)")
        .bind(environment_id)
        .bind(session_id)
        .bind(public.to_string())
        .bind(digest(&token))
        .execute(&state.store.0)
        .await?;
    let request = ProvisionRequest {
        environment_id: environment_id.to_owned(),
        registry_url,
        token,
        container_size: hosted.container_size,
        packages: hosted.packages,
        env: hosted.env,
        setup_commands: hosted.setup_commands,
    };
    Ok((public, request))
}

/// Provision the environment in the background. A sandbox that comes up after
/// its session was deleted is destroyed at once.
pub(crate) fn start(state: &Arc<State>, session_id: String, request: ProvisionRequest) {
    let Some(provider) = state.provider() else {
        return;
    };
    let state = Arc::clone(state);
    tokio::spawn(async move {
        let environment_id = request.environment_id.clone();
        match provider.provision(request).await {
            Ok(sandbox_id) => {
                let recorded = sqlx::query(
                    "UPDATE environments SET sandbox_id = ?, provisioned = 1 WHERE id = ?",
                )
                .bind(&sandbox_id)
                .bind(&environment_id)
                .execute(&state.store.0)
                .await;
                match recorded {
                    Ok(done) if done.rows_affected() == 1 => {
                        crate::environments::emit(
                            &state,
                            &session_id,
                            &environment_id,
                            "openai_hosted",
                            "ready",
                            /*error*/ None,
                        );
                    }
                    _ => {
                        if let Err(error) = provider.destroy(sandbox_id).await {
                            tracing::warn!(
                                error = format!("{error:#}"),
                                "orphaned sandbox not destroyed"
                            );
                        }
                    }
                }
            }
            Err(error) => {
                let message = format!("provisioning failed: {error:#}");
                tracing::warn!(
                    environment_id,
                    error = message,
                    "environment provisioning failed"
                );
                if let Err(error) = crate::environments::fail(
                    &state,
                    &session_id,
                    &environment_id,
                    "environment_provisioning_failed",
                    &message,
                )
                .await
                {
                    tracing::warn!(
                        error = format!("{error:#}"),
                        "provisioning failure not recorded"
                    );
                }
            }
        }
    });
}

/// Destroy a deleted session's sandbox, when it had one.
pub(crate) fn destroy(state: &State, sandbox_id: String) {
    let Some(provider) = state.provider() else {
        return;
    };
    tokio::spawn(async move {
        if let Err(error) = provider.destroy(sandbox_id).await {
            tracing::warn!(error = format!("{error:#}"), "sandbox not destroyed");
        }
    });
}

/// Fail environments whose provisioning a restart interrupted: their
/// confidential setup was never stored, so it cannot run again.
pub(crate) async fn recover(state: &State) -> anyhow::Result<()> {
    sqlx::query("UPDATE environments SET failed = 1 WHERE provider IS NOT NULL AND provisioned = 0 AND failed = 0")
        .execute(&state.store.0)
        .await?;
    Ok(())
}

#[cfg(test)]
#[path = "managed_tests.rs"]
mod tests;
