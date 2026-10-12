//! Skills from the Skills API installed into self-hosted sessions.
//!
//! This is a local extension. The contract takes `skill_reference` skills only
//! for OpenAI-hosted environments, which this service does not offer; here a
//! `self_hosted` environment accepts the same `skills` list. Each reference
//! resolves, in the caller's project, to a concrete version when the session
//! is created, and that version's contents are pinned beside the session, so
//! deleting the skill or version later changes nothing the session installs.
//!
//! Before each turn the pinned skills are written to the executor under
//! [`INSTALL_DIRECTORY`] in the workspace, one folder per skill name, unless a
//! marker shows the same pins are already there. The directory is one more
//! capability root of the session's thread, so Codex discovers the skills as
//! it does those in `capability_directories`. Executable bits are not kept:
//! the executor filesystem API writes contents only.
use crate::ApiError;
use crate::State;
use crate::contract::invalid;
use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::ReadFileOptions;
use codex_exec_server::WriteFileOptions;
use codex_utils_path_uri::PathUri;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashSet;
use std::io::Read;
use std::path::PathBuf;

/// Where pinned skills are installed, relative to the workspace directory.
pub(crate) const INSTALL_DIRECTORY: &str = ".agents-api/skills";
/// Records which pins an installation holds.
const MARKER: &str = ".installed";
/// The most skills one session may install.
const MAX_SKILLS: usize = 32;

/// A requested skill: its ID and an optional version selector.
pub(crate) struct Reference {
    skill_id: String,
    version: Option<String>,
}

/// A reference resolved to a concrete version.
pub(crate) struct Resolved {
    skill_id: String,
    version: i64,
    version_id: String,
    name: String,
    description: String,
    sha256: String,
}

/// The `skills` of a `self_hosted` environment parameter.
pub(crate) fn references(skills: &Value) -> Result<Vec<Reference>, ApiError> {
    let skills = match skills {
        Value::Null => return Ok(Vec::new()),
        Value::Array(skills) => skills,
        _ => return Err(invalid("environment.skills must be a list")),
    };
    if skills.len() > MAX_SKILLS {
        return Err(invalid(format!(
            "environment.skills accepts at most {MAX_SKILLS} skills"
        )));
    }
    skills
        .iter()
        .map(|skill| match skill["type"].as_str() {
            Some("skill_reference") => {
                if let Some(unknown) = skill.as_object().into_iter().flatten().find_map(|(key, _)| {
                    (!matches!(key.as_str(), "type" | "skill_id" | "version")).then_some(key)
                }) {
                    return Err(invalid(format!("unknown field in skill reference: {unknown}")));
                }
                let skill_id = skill["skill_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| invalid("skill references need a skill_id"))?;
                let version = match &skill["version"] {
                    Value::Null => None,
                    Value::String(version) => Some(version.clone()),
                    _ => {
                        return Err(invalid(
                            "skill reference versions must be a version number or latest",
                        ));
                    }
                };
                Ok(Reference {
                    skill_id: skill_id.to_owned(),
                    version,
                })
            }
            Some("inline") => Err(invalid(
                "inline skills are not supported for self_hosted environments; upload the skill with the Skills API and reference it",
            )),
            _ => Err(invalid("environment.skills entries must have type skill_reference")),
        })
        .collect()
}

/// Resolve references in the caller's project: no version means the default,
/// `latest` the newest, and a number that version. Installed folders are
/// named after skills, so two skills with one name are refused.
pub(crate) async fn resolve(
    state: &State,
    project_id: &str,
    references: Vec<Reference>,
) -> Result<Vec<Resolved>, ApiError> {
    let mut resolved = Vec::with_capacity(references.len());
    let mut names = HashSet::new();
    for Reference { skill_id, version } in references {
        let selected: Option<i64> = sqlx::query_scalar(
            "SELECT CASE WHEN ?3 IS NULL THEN default_version \
                 WHEN ?3 = 'latest' THEN (SELECT MAX(version) FROM skill_versions WHERE skill_id = s.id) \
                 ELSE CAST(?3 AS INTEGER) END \
             FROM skills s WHERE id = ?1 AND project_id = ?2",
        )
        .bind(&skill_id)
        .bind(project_id)
        .bind(&version)
        .fetch_optional(&state.store.0)
        .await
        .map_err(anyhow::Error::from)?;
        let selected = selected.ok_or_else(|| invalid(format!("skill {skill_id} not found")))?;
        let requested = version.as_deref().unwrap_or("default");
        let valid = version.as_deref().is_none_or(|version| {
            version == "latest"
                || (version.parse::<i64>().is_ok_and(|number| number > 0)
                    && !version.starts_with('+'))
        });
        let row: Option<(String, String, String, String)> = if valid {
            sqlx::query_as(
                "SELECT id, name, description, sha256 FROM skill_versions WHERE skill_id = ? AND version = ?",
            )
            .bind(&skill_id)
            .bind(selected)
            .fetch_optional(&state.store.0)
            .await
            .map_err(anyhow::Error::from)?
        } else {
            None
        };
        let (version_id, name, description, sha256) =
            row.ok_or_else(|| invalid(format!("skill {skill_id} has no version {requested}")))?;
        if !names.insert(name.clone()) {
            return Err(invalid(format!(
                "environment.skills names two skills called {name}"
            )));
        }
        resolved.push(Resolved {
            skill_id,
            version: selected,
            version_id,
            name,
            description,
            sha256,
        });
    }
    Ok(resolved)
}

fn pins(state: &State, session_id: &str) -> PathBuf {
    state.skill_pins.join(session_id)
}

/// Pin resolved skills to a new session: record them and link (or copy) each
/// version's contents beside the session.
pub(crate) async fn pin(
    state: &State,
    session_id: &str,
    resolved: &[Resolved],
) -> Result<(), ApiError> {
    if resolved.is_empty() {
        return Ok(());
    }
    let directory = pins(state, session_id);
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(anyhow::Error::from)?;
    for (position, skill) in resolved.iter().enumerate() {
        let source = state.skills.contents(&skill.version_id);
        let pinned = directory.join(format!("{position}.zip"));
        if tokio::fs::hard_link(&source, &pinned).await.is_err() {
            tokio::fs::copy(&source, &pinned).await.map_err(|_| {
                invalid(format!(
                    "skill {} version {} was deleted while the session was created",
                    skill.skill_id, skill.version
                ))
            })?;
        }
        sqlx::query("INSERT INTO session_skills (session_id, position, skill_id, version, name, description, sha256) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(session_id).bind(position as i64).bind(&skill.skill_id).bind(skill.version)
            .bind(&skill.name).bind(&skill.description).bind(&skill.sha256)
            .execute(&state.store.0).await.map_err(anyhow::Error::from)?;
    }
    Ok(())
}

/// A session's pinned skills as `EnvironmentInfo.skills` entries.
pub(crate) async fn installed(state: &State, session_id: &str) -> anyhow::Result<Vec<Value>> {
    let rows: Vec<(String, i64, String, String)> = sqlx::query_as(
        "SELECT skill_id, version, name, description FROM session_skills WHERE session_id = ? ORDER BY position",
    )
    .bind(session_id)
    .fetch_all(&state.store.0)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(skill_id, version, name, description)| {
            json!({"type": "skill_reference", "skill_id": skill_id, "version": version.to_string(),
                "name": name, "description": description})
        })
        .collect())
}

/// The capability root a session's skills are installed in, if it has any.
pub(crate) async fn root(
    state: &State,
    session_id: &str,
    workspace_directory: &str,
) -> Result<Option<String>, ApiError> {
    let any: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM session_skills WHERE session_id = ?)")
            .bind(session_id)
            .fetch_one(&state.store.0)
            .await
            .map_err(anyhow::Error::from)?;
    if !any {
        return Ok(None);
    }
    Ok(Some(
        install_root(workspace_directory)?.inferred_native_path_string(),
    ))
}

fn install_root(workspace_directory: &str) -> Result<PathUri, ApiError> {
    crate::environment_files::path_uri(workspace_directory)
        .and_then(|workspace| workspace.join_descendant(INSTALL_DIRECTORY).ok())
        .ok_or_else(|| anyhow::anyhow!("stored workspace directory is not absolute").into())
}

/// Write a session's pinned skills to its executor unless the marker shows
/// they are already there. A session's pins never change, so writing every
/// file again repairs an installation that was wiped or altered; files added
/// beside them are left alone.
pub(crate) async fn install(
    state: &State,
    session_id: &str,
    environment_id: &str,
) -> Result<(), ApiError> {
    let rows: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT position, name, sha256 FROM session_skills WHERE session_id = ? ORDER BY position",
    )
    .bind(session_id)
    .fetch_all(&state.store.0)
    .await
    .map_err(anyhow::Error::from)?;
    if rows.is_empty() {
        return Ok(());
    }
    let workspace = crate::environment_files::Workspace::open(state, environment_id).await?;
    let root = workspace
        .root
        .join_descendant(INSTALL_DIRECTORY)
        .map_err(anyhow::Error::from)?;
    let marker = root.join_descendant(MARKER).map_err(anyhow::Error::from)?;
    let mut hasher = Sha256::new();
    for (_, name, sha256) in &rows {
        hasher.update(format!("{name}:{sha256}\n"));
    }
    let digest = format!("{:x}", hasher.finalize());
    let current = workspace
        .files
        .read_file(&marker, ReadFileOptions::default(), /*sandbox*/ None)
        .await
        .ok();
    if current.as_deref() == Some(digest.as_bytes()) {
        return Ok(());
    }
    let failed = |error: std::io::Error| {
        ApiError(
            axum::http::StatusCode::BAD_GATEWAY,
            format!("installing the session's skills on the executor failed: {error}"),
        )
    };
    let directories = CreateDirectoryOptions {
        recursive: true,
        follow_symlinks: false,
    };
    // Never follow a link out of the installation.
    let write = WriteFileOptions {
        follow_symlinks: false,
    };
    for (position, name, sha256) in rows {
        let pinned = pins(state, session_id).join(format!("{position}.zip"));
        crate::files::verify(&pinned, &sha256).await?;
        let skill = root.join_descendant(&name).map_err(anyhow::Error::from)?;
        // Entries are read one at a time off the runtime and written as they
        // arrive, so at most two files are held at once.
        let (sender, mut receiver) =
            tokio::sync::mpsc::channel::<anyhow::Result<(String, Vec<u8>)>>(1);
        let reader = tokio::task::spawn_blocking(move || {
            let entries = || -> anyhow::Result<()> {
                let mut zip = zip::ZipArchive::new(std::fs::File::open(&pinned)?)?;
                for index in 0..zip.len() {
                    let mut entry = zip.by_index(index)?;
                    // Canonical archives hold one top-level folder; files go
                    // under the skill's own name instead.
                    let Some((_, path)) = entry.name().split_once('/') else {
                        continue;
                    };
                    let path = path.to_owned();
                    let mut contents = Vec::with_capacity(entry.size() as usize);
                    entry.read_to_end(&mut contents)?;
                    if sender.blocking_send(Ok((path, contents))).is_err() {
                        return Ok(());
                    }
                }
                Ok(())
            };
            if let Err(error) = entries() {
                let _ = sender.blocking_send(Err(error));
            }
        });
        while let Some(entry) = receiver.recv().await {
            let (path, contents) = entry?;
            let destination = skill.join_descendant(&path).map_err(anyhow::Error::from)?;
            if let Some(parent) = destination.parent() {
                workspace
                    .files
                    .create_directory(&parent, directories, /*sandbox*/ None)
                    .await
                    .map_err(failed)?;
            }
            workspace
                .files
                .write_file(&destination, contents, write, /*sandbox*/ None)
                .await
                .map_err(failed)?;
        }
        reader.await.map_err(anyhow::Error::from)?;
    }
    workspace
        .files
        .write_file(&marker, digest.into_bytes(), write, /*sandbox*/ None)
        .await
        .map_err(failed)?;
    Ok(())
}

/// Remove a deleted session's pinned contents. A failure leaves them for the
/// startup sweep.
pub(crate) async fn forget(state: &State, session_id: &str) {
    let _ = tokio::fs::remove_dir_all(pins(state, session_id)).await;
}

/// At startup, remove pins of sessions that no longer exist.
pub(crate) async fn sweep(state: &State) -> anyhow::Result<()> {
    let Ok(mut entries) = tokio::fs::read_dir(&state.skill_pins).await else {
        return Ok(());
    };
    while let Some(entry) = entries.next_entry().await? {
        let session_id = entry.file_name().to_string_lossy().into_owned();
        let pinned: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM session_skills WHERE session_id = ?)")
                .bind(&session_id)
                .fetch_one(&state.store.0)
                .await?;
        if !pinned {
            let _ = tokio::fs::remove_dir_all(entry.path()).await;
        }
    }
    Ok(())
}
