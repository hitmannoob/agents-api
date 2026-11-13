//! Projects and project API keys.
//!
//! Every API key belongs to exactly one project and carries a set of
//! permissions. Requests authenticate with a key; the key implies the project,
//! and no request names a project itself. The operator token
//! (`CODEX_AGENTS_API_TOKEN`) authenticates as the default project with every
//! permission, so deployments that predate projects keep working.
//!
//! Operators manage projects and keys with the CLI through [`Admin`], which
//! touches only the `projects` and `api_keys` tables. The server reads keys on
//! each request, so a created or revoked key takes effect without a restart.

use crate::State;
use anyhow::Context;
use base64::Engine;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use sqlx::SqlitePool;
use uuid::Uuid;

/// The project that owns rows created before projects existed, and the one the
/// operator token authenticates as.
pub const DEFAULT_PROJECT: &str = "proj_default";

/// Every permission a key can hold. Agents, Responses, Vaults, and Files names
/// follow the official guides. The guides name no string for webhooks, skills,
/// or the environment key's "connect environments" permission, so
/// `api.webhooks.*`, `api.skills.*`, and `api.environments.connect` are local
/// choices.
pub const PERMISSIONS: [&str; 12] = [
    "api.agents.read",
    "api.agents.write",
    "api.responses.write",
    "api.vaults.read",
    "api.vaults.write",
    "api.files.read",
    "api.files.write",
    "api.webhooks.read",
    "api.webhooks.write",
    "api.skills.read",
    "api.skills.write",
    "api.environments.connect",
];

/// The authenticated caller of one request, stored in the request extensions.
#[derive(Clone, Debug)]
pub(crate) struct Principal {
    pub project_id: String,
    permissions: Vec<String>,
    pub(crate) is_operator: bool,
}

impl Principal {
    /// The first permission in `required` that this caller lacks.
    pub(crate) fn missing(&self, required: &[&'static str]) -> Option<&'static str> {
        required
            .iter()
            .copied()
            .find(|permission| !self.permissions.iter().any(|held| held == permission))
    }
}

fn digest(secret: &str) -> String {
    format!("{:x}", Sha256::digest(secret.as_bytes()))
}

/// The caller a bearer secret authenticates, or `None` for an unknown or
/// revoked key.
pub(crate) async fn resolve(state: &State, secret: &str) -> anyhow::Result<Option<Principal>> {
    let presented = digest(secret);
    // Compare digests rather than secrets so the comparison time does not
    // depend on how much of the operator token a guess matches.
    if presented == digest(&state.token) {
        return Ok(Some(Principal {
            project_id: DEFAULT_PROJECT.to_owned(),
            is_operator: true,
            permissions: PERMISSIONS
                .iter()
                .map(|&permission| permission.to_owned())
                .collect(),
        }));
    }
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT project_id, permissions FROM api_keys WHERE secret_sha256 = ? AND revoked_at IS NULL",
    )
    .bind(presented)
    .fetch_optional(&state.store.0)
    .await?;
    row.map(|(project_id, permissions)| {
        Ok(Principal {
            project_id,
            is_operator: false,
            permissions: serde_json::from_str(&permissions)?,
        })
    })
    .transpose()
}

/// Root resources whose ownership also authorizes their nested resources.
#[derive(Clone, Copy)]
pub(crate) enum Resource {
    Agent,
    Session,
    PublicSession,
    Environment,
    Vault,
    Webhook,
    File,
    Skill,
}

impl Resource {
    fn query(self) -> &'static str {
        match self {
            Self::Agent => "SELECT project_id FROM agents WHERE id = ?",
            Self::Session => "SELECT project_id FROM sessions WHERE id = ?",
            Self::PublicSession => "SELECT project_id FROM public_sessions WHERE id = ?",
            Self::Environment => {
                "SELECT p.project_id FROM environments e JOIN public_sessions p ON p.id = e.session_id WHERE e.id = ?"
            }
            Self::Vault => "SELECT project_id FROM vaults WHERE id = ?",
            Self::Webhook => "SELECT project_id FROM webhook_endpoints WHERE id = ?",
            Self::File => "SELECT project_id FROM files WHERE id = ?",
            Self::Skill => "SELECT project_id FROM skills WHERE id = ?",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::Agent => "agent not found",
            Self::Session | Self::PublicSession => "session not found",
            Self::Environment => "environment not found",
            Self::Vault => "vault not found",
            Self::Webhook => "webhook endpoint not found",
            Self::File => "file not found",
            Self::Skill => "skill not found",
        }
    }
}

/// Check the decoded ID inside a handler, independently of route middleware.
/// Missing and foreign roots have the same status and message. Ownership is
/// immutable, so a successful check also scopes subsequent child operations.
pub(crate) async fn require(
    state: &State,
    project_id: &str,
    id: &str,
    resource: Resource,
) -> Result<(), crate::ApiError> {
    let owner: Option<String> = sqlx::query_scalar(resource.query())
        .bind(id)
        .fetch_optional(&state.store.0)
        .await
        .map_err(anyhow::Error::from)?;
    if owner.as_deref() != Some(project_id) {
        return Err(crate::ApiError(
            axum::http::StatusCode::NOT_FOUND,
            resource.message().into(),
        ));
    }
    Ok(())
}

/// Resource selection uses only Axum's matched template, never the raw URI.
/// IDs come from Axum's Path extractor, shared with the actual handlers.
pub(crate) fn root(path: &str) -> Option<(Resource, &'static str)> {
    match path
        .strip_prefix("/v1/")?
        .split('/')
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["agents", "sessions", "{id}", ..] => Some((Resource::PublicSession, "id")),
        ["agents", "environments", "{id}", ..] => Some((Resource::Environment, "id")),
        ["agents", "{id}", ..] | ["prototype", "agents", "{id}", ..] => {
            Some((Resource::Agent, "id"))
        }
        ["sessions", "{id}", ..] => Some((Resource::Session, "id")),
        ["vaults", "{vault_id}", ..] => Some((Resource::Vault, "vault_id")),
        ["webhook_endpoints", "{id}", ..] => Some((Resource::Webhook, "id")),
        ["files", "{id}", ..] => Some((Resource::File, "id")),
        ["skills", "{id}", ..] => Some((Resource::Skill, "id")),
        _ => None,
    }
}

/// Reject malformed escapes even though percent-encoding's decoder leaves
/// them literal. Decode exactly once via Axum; do not reinterpret decoded
/// separators, residual escapes, dot segments, or control characters as IDs.
pub(crate) fn unambiguous_path(
    raw: &str,
    params: &std::collections::HashMap<String, String>,
) -> bool {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit()
            {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    params.values().all(|id| {
        !id.is_empty()
            && id != "."
            && id != ".."
            && !id
                .chars()
                .any(|c| matches!(c, '/' | '\\' | '%') || c.is_control())
    })
}

/// The project that owns an environment, through its session.
pub(crate) async fn environment_project(
    state: &State,
    environment_id: &str,
) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT p.project_id FROM environments e JOIN public_sessions p ON p.id = e.session_id WHERE e.id = ?",
    )
    .bind(environment_id)
    .fetch_optional(&state.store.0)
    .await?)
}

/// The permissions an operation requires, from its method and path. Reads need
/// the resource's read permission and every other method its write
/// permission; operations that run model turns also need
/// `api.responses.write`. Unknown paths need none, so they answer 404.
pub(crate) fn required(method: &axum::http::Method, path: &str) -> &'static [&'static str] {
    let read = method == axum::http::Method::GET || method == axum::http::Method::HEAD;
    let under = |prefix: &str| path == prefix || path.starts_with(&format!("{prefix}/"));
    if under("/v1/vaults") {
        if read {
            &["api.vaults.read"]
        } else {
            &["api.vaults.write"]
        }
    } else if under("/v1/files") {
        if read {
            &["api.files.read"]
        } else {
            &["api.files.write"]
        }
    } else if under("/v1/skills") {
        if read {
            &["api.skills.read"]
        } else {
            &["api.skills.write"]
        }
    } else if under("/v1/webhook_endpoints") || under("/v1/webhook_event_types") {
        if read {
            &["api.webhooks.read"]
        } else {
            &["api.webhooks.write"]
        }
    } else if under("/v1/agents") || under("/v1/sessions") || under("/v1/prototype/agents") {
        let runs_turns = path == "/v1/agents/sessions"
            || path == "/v1/sessions"
            || (path.starts_with("/v1/agents/sessions/") && path.ends_with("/events"))
            || (path.starts_with("/v1/sessions/") && path.ends_with("/input"));
        if read {
            &["api.agents.read"]
        } else if runs_turns {
            &["api.agents.write", "api.responses.write"]
        } else {
            &["api.agents.write"]
        }
    } else {
        &[]
    }
}

#[derive(Debug, Serialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub created_at: i64,
}

#[derive(Debug, Serialize)]
pub struct ApiKey {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub redacted_value: String,
    pub permissions: Vec<String>,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
}

/// A newly created key and its secret, which is never retrievable again.
#[derive(Debug, Serialize)]
pub struct CreatedKey {
    #[serde(flatten)]
    pub key: ApiKey,
    pub value: String,
}

/// An `api_keys` row: ID, project, name, redacted value, permissions JSON,
/// creation time, and revocation time.
type KeyRow = (String, String, String, String, String, i64, Option<i64>);

/// Operator administration of projects and keys over an API data directory.
///
/// Unlike the server's store, this takes no data-directory lock and runs no
/// startup recovery, so it can run beside a serving process; SQLite serializes
/// the writes.
pub struct Admin(SqlitePool);

impl Admin {
    pub async fn open(directory: AbsolutePathBuf) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(&directory).await?;
        let path = directory.join("agents-api.sqlite");
        let pool = SqliteConfig::from_sqlite_home(directory)
            .open_read_write_pool(path.as_path())
            .await?;
        crate::store::MIGRATOR.run(&pool).await?;
        Ok(Self(pool))
    }

    pub async fn create_project(&self, name: &str) -> anyhow::Result<Project> {
        let name = name.trim();
        anyhow::ensure!(
            !name.is_empty() && name.chars().count() <= 256,
            "project name must contain 1 to 256 characters"
        );
        let project = Project {
            id: format!("proj_{}", Uuid::new_v4().simple()),
            name: name.to_owned(),
            created_at: crate::contract::now() as i64,
        };
        sqlx::query(
            "INSERT INTO projects (id, name, created_at, created_seq) \
             SELECT ?, ?, ?, COALESCE(MAX(created_seq), 0) + 1 FROM projects",
        )
        .bind(&project.id)
        .bind(&project.name)
        .bind(project.created_at)
        .execute(&self.0)
        .await?;
        Ok(project)
    }

    pub async fn projects(&self) -> anyhow::Result<Vec<Project>> {
        let rows: Vec<(String, String, i64)> =
            sqlx::query_as("SELECT id, name, created_at FROM projects ORDER BY created_seq")
                .fetch_all(&self.0)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(id, name, created_at)| Project {
                id,
                name,
                created_at,
            })
            .collect())
    }

    /// Create a key in `project_id` holding `permissions`, which must be
    /// non-empty and drawn from [`PERMISSIONS`].
    pub async fn create_key(
        &self,
        project_id: &str,
        name: &str,
        permissions: &[String],
    ) -> anyhow::Result<CreatedKey> {
        anyhow::ensure!(
            !permissions.is_empty(),
            "a key needs at least one permission"
        );
        for permission in permissions {
            anyhow::ensure!(
                PERMISSIONS.contains(&permission.as_str()),
                "unknown permission {permission}; expected one of {}",
                PERMISSIONS.join(", ")
            );
        }
        let mut permissions = permissions.to_vec();
        permissions
            .sort_by_key(|permission| PERMISSIONS.iter().position(|known| known == permission));
        permissions.dedup();
        let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM projects WHERE id = ?")
            .bind(project_id)
            .fetch_optional(&self.0)
            .await?;
        exists.with_context(|| format!("no project {project_id}"))?;
        let value = format!(
            "sk-proj-{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
        );
        let key = ApiKey {
            id: format!("key_{}", Uuid::new_v4().simple()),
            project_id: project_id.to_owned(),
            name: name.trim().to_owned(),
            redacted_value: format!("sk-proj-...{}", &value[value.len() - 4..]),
            permissions,
            created_at: crate::contract::now() as i64,
            revoked_at: None,
        };
        sqlx::query(
            "INSERT INTO api_keys (id, project_id, name, secret_sha256, redacted_value, permissions, created_at, created_seq) \
             SELECT ?, ?, ?, ?, ?, ?, ?, COALESCE(MAX(created_seq), 0) + 1 FROM api_keys",
        )
        .bind(&key.id)
        .bind(&key.project_id)
        .bind(&key.name)
        .bind(digest(&value))
        .bind(&key.redacted_value)
        .bind(serde_json::to_string(&key.permissions)?)
        .bind(key.created_at)
        .execute(&self.0)
        .await?;
        Ok(CreatedKey { key, value })
    }

    /// Keys in creation order, optionally only one project's.
    pub async fn keys(&self, project_id: Option<&str>) -> anyhow::Result<Vec<ApiKey>> {
        let rows: Vec<KeyRow> = sqlx::query_as(
            "SELECT id, project_id, name, redacted_value, permissions, created_at, revoked_at \
             FROM api_keys WHERE ? IS NULL OR project_id = ? ORDER BY created_seq",
        )
        .bind(project_id)
        .bind(project_id)
        .fetch_all(&self.0)
        .await?;
        rows.into_iter()
            .map(
                |(id, project_id, name, redacted_value, permissions, created_at, revoked_at)| {
                    Ok(ApiKey {
                        id,
                        project_id,
                        name,
                        redacted_value,
                        permissions: serde_json::from_str(&permissions)?,
                        created_at,
                        revoked_at,
                    })
                },
            )
            .collect()
    }

    /// Revoke a key. Requests that present it fail from then on; revoking an
    /// already revoked key keeps its original revocation time.
    pub async fn revoke_key(&self, id: &str) -> anyhow::Result<()> {
        let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM api_keys WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.0)
            .await?;
        exists.with_context(|| format!("no API key {id}"))?;
        sqlx::query("UPDATE api_keys SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
            .bind(crate::contract::now() as i64)
            .bind(id)
            .execute(&self.0)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "projects_tests.rs"]
mod tests;
