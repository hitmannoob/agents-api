//! Encrypted storage for credential values, under a passphrase the operator
//! supplies.
//!
//! The passphrase unlocks a random X25519 key once, at startup: the key is kept
//! in `secrets/agents-api.key`, encrypted with age's scrypt passphrase mode,
//! which is deliberately slow and memory-hard (about a second and a gibibyte).
//! Each value is then stored as its own database row, encrypted to that key
//! with age, so reads and writes cost microseconds. The row's plaintext names
//! the secret, so a row copied under another name does not decrypt as it.
//!
//! Stores written before this layout kept every value in one file
//! (`secrets/local.age`) that each write re-encrypted with the passphrase, so
//! every operation paid the key derivation. The first configured start moves
//! their values into rows and removes the file.
use crate::ApiError;
use age::secrecy::ExposeSecret;
use age::secrecy::SecretString;
use anyhow::Context;
use axum::http::StatusCode;
use serde_json::Value;
use serde_json::json;
use sqlx::SqlitePool;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;

const KEY_FILE: &str = "agents-api.key";
const LEGACY_FILE: &str = "local.age";

struct Unlocked {
    identity: age::x25519::Identity,
    recipient: age::x25519::Recipient,
    pool: SqlitePool,
}

/// Unconfigured until the operator supplies a passphrase.
#[derive(Default)]
pub(crate) struct Secrets(OnceLock<Unlocked>);

impl Secrets {
    /// Enable the store once the passphrase is shown to unlock the stored key
    /// (or the legacy file), so a wrong passphrase stops startup instead of
    /// failing every later request. Only the first accepted passphrase takes
    /// effect. Returns the names already stored, or none when already
    /// configured.
    pub(crate) async fn configure(
        &self,
        directory: std::path::PathBuf,
        pool: SqlitePool,
        passphrase: String,
    ) -> anyhow::Result<Vec<String>> {
        let secrets = directory.join("secrets");
        let refused = format!(
            "the vault passphrase cannot read the secrets stored in {}",
            secrets.display()
        );
        let (identity, legacy) =
            tokio::task::spawn_blocking(move || unlock(&secrets, SecretString::from(passphrase)))
                .await?
                .context(refused)?;
        // Rows a previous migration already wrote are kept: they are as new
        // as the file, which is removed only after they commit.
        if !legacy.is_empty() {
            let mut tx = pool.begin().await?;
            for (name, value) in &legacy {
                sqlx::query("INSERT OR IGNORE INTO secret_values (name, ciphertext) VALUES (?, ?)")
                    .bind(name)
                    .bind(seal(&identity.to_public(), name, value)?)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            tracing::info!(
                migrated = legacy.len(),
                "moved secrets out of the passphrase-encrypted file"
            );
        }
        let legacy_file = directory.join("secrets").join(LEGACY_FILE);
        if legacy_file.exists() {
            std::fs::remove_file(&legacy_file)
                .with_context(|| format!("failed to remove {}", legacy_file.display()))?;
        }
        let stored = sqlx::query_scalar("SELECT name FROM secret_values")
            .fetch_all(&pool)
            .await?;
        let recipient = identity.to_public();
        let configured = self
            .0
            .set(Unlocked {
                identity,
                recipient,
                pool,
            })
            .is_ok();
        Ok(if configured { stored } else { Vec::new() })
    }

    /// Remove the secrets of records already deleted. If that fails, for
    /// example without a passphrase, the startup sweep removes them later.
    pub(crate) async fn discard(&self, names: Vec<String>) {
        // Nothing was stored without a passphrase.
        if names.is_empty() || !self.configured() {
            return;
        }
        if let Err(error) = self.delete(names).await {
            tracing::warn!(
                error = error.1,
                "secrets of deleted records left for the startup sweep"
            );
        }
    }

    fn unlocked(&self) -> Result<&Unlocked, ApiError> {
        self.0.get().ok_or_else(|| {
            ApiError(
                StatusCode::NOT_IMPLEMENTED,
                "vault credentials require the operator to configure a vault passphrase".into(),
            )
        })
    }

    /// Whether the operator supplied a passphrase.
    pub(crate) fn configured(&self) -> bool {
        self.0.get().is_some()
    }

    pub(crate) async fn set(&self, name: String, value: Value) -> Result<(), ApiError> {
        let unlocked = self.unlocked()?;
        let ciphertext = seal(&unlocked.recipient, &name, &value)?;
        sqlx::query(
            "INSERT INTO secret_values (name, ciphertext) VALUES (?, ?) \
             ON CONFLICT(name) DO UPDATE SET ciphertext = excluded.ciphertext",
        )
        .bind(&name)
        .bind(ciphertext)
        .execute(&unlocked.pool)
        .await
        .map_err(anyhow::Error::from)?;
        Ok(())
    }

    pub(crate) async fn get(&self, name: String) -> Result<Option<Value>, ApiError> {
        let unlocked = self.unlocked()?;
        let ciphertext: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT ciphertext FROM secret_values WHERE name = ?")
                .bind(&name)
                .fetch_optional(&unlocked.pool)
                .await
                .map_err(anyhow::Error::from)?;
        Ok(ciphertext
            .map(|ciphertext| open(&unlocked.identity, &name, &ciphertext))
            .transpose()?)
    }

    pub(crate) async fn delete(&self, names: Vec<String>) -> Result<(), ApiError> {
        let unlocked = self.unlocked()?;
        let mut tx = unlocked.pool.begin().await.map_err(anyhow::Error::from)?;
        for name in names {
            sqlx::query("DELETE FROM secret_values WHERE name = ?")
                .bind(name)
                .execute(&mut *tx)
                .await
                .map_err(anyhow::Error::from)?;
        }
        tx.commit().await.map_err(anyhow::Error::from)?;
        Ok(())
    }
}

/// Read the stored key with the passphrase, or create one, and read the
/// legacy file's values if it is still there. Runs the slow key derivation at
/// most twice (the key and the legacy file).
fn unlock(
    directory: &Path,
    passphrase: SecretString,
) -> anyhow::Result<(age::x25519::Identity, BTreeMap<String, Value>)> {
    let legacy_path = directory.join(LEGACY_FILE);
    let legacy = if legacy_path.exists() {
        let plaintext = age::decrypt(
            &age::scrypt::Identity::new(passphrase.clone()),
            &std::fs::read(&legacy_path)?,
        )?;
        // codex-secrets' file: {"version", "secrets": {"global/<NAME>": "<JSON>"}}.
        let file: Value = serde_json::from_slice(&plaintext)?;
        let mut values = BTreeMap::new();
        for (key, value) in file["secrets"].as_object().into_iter().flatten() {
            let (Some(name), Some(value)) = (key.strip_prefix("global/"), value.as_str()) else {
                continue;
            };
            values.insert(name.to_owned(), serde_json::from_str(value)?);
        }
        values
    } else {
        BTreeMap::new()
    };
    let key_path = directory.join(KEY_FILE);
    let identity = if key_path.exists() {
        let plaintext = age::decrypt(
            &age::scrypt::Identity::new(passphrase),
            &std::fs::read(&key_path)?,
        )?;
        String::from_utf8(plaintext)?
            .parse::<age::x25519::Identity>()
            .map_err(|error| anyhow::anyhow!("the stored secrets key is malformed: {error}"))?
    } else {
        let identity = age::x25519::Identity::generate();
        let ciphertext = age::encrypt(
            &age::scrypt::Recipient::new(passphrase),
            identity.to_string().expose_secret().as_bytes(),
        )?;
        std::fs::create_dir_all(directory)?;
        let partial = directory.join(format!("{KEY_FILE}.tmp"));
        std::fs::write(&partial, ciphertext)?;
        #[cfg(unix)]
        std::fs::set_permissions(
            &partial,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )?;
        std::fs::rename(&partial, &key_path)?;
        identity
    };
    Ok((identity, legacy))
}

fn seal(recipient: &age::x25519::Recipient, name: &str, value: &Value) -> anyhow::Result<Vec<u8>> {
    let plaintext = json!({"name": name, "value": value}).to_string();
    Ok(age::encrypt(recipient, plaintext.as_bytes())?)
}

fn open(identity: &age::x25519::Identity, name: &str, ciphertext: &[u8]) -> anyhow::Result<Value> {
    let mut sealed: Value = serde_json::from_slice(&age::decrypt(identity, ciphertext)?)?;
    anyhow::ensure!(
        sealed["name"] == name,
        "the stored secret {name} was sealed under another name"
    );
    Ok(sealed["value"].take())
}

/// The secrets-store name holding a vault credential's values.
pub(crate) fn credential_name(credential_id: &str) -> String {
    format!("VAULT_{}", credential_id.to_ascii_uppercase())
}

#[cfg(test)]
#[path = "secrets_tests.rs"]
mod tests;
