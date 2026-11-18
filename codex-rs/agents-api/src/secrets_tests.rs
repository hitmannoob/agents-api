use super::*;
use pretty_assertions::assert_eq;
use std::sync::Arc;

/// `ApiError` is not a `std::error::Error`; tests report its message.
fn api(error: ApiError) -> anyhow::Error {
    anyhow::anyhow!(error.1)
}

const PASSPHRASE: &str = "operator-passphrase-for-the-secrets-tests-0123";

/// Supplies the passphrase to codex-secrets, which wrote the legacy file.
#[derive(Debug)]
struct Legacy;

impl codex_keyring_store::KeyringStore for Legacy {
    fn load(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Option<String>, codex_keyring_store::CredentialStoreError> {
        Ok(Some(PASSPHRASE.to_owned()))
    }

    fn save(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(), codex_keyring_store::CredentialStoreError> {
        Ok(())
    }

    fn delete(&self, _: &str, _: &str) -> Result<bool, codex_keyring_store::CredentialStoreError> {
        Ok(false)
    }
}

async fn pool(directory: &Path) -> anyhow::Result<SqlitePool> {
    let store = crate::store::Store::open(
        codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(directory)?,
    )
    .await?;
    Ok(store.0)
}

#[tokio::test]
async fn values_are_rows_sealed_to_the_unlocked_key() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let pool = pool(directory.path()).await?;
    let secrets = Secrets::default();
    assert_eq!(
        secrets
            .configure(
                directory.path().to_path_buf(),
                pool.clone(),
                PASSPHRASE.into()
            )
            .await?,
        Vec::<String>::new()
    );
    secrets
        .set("VAULT_A".into(), json!({"token": "tok-1"}))
        .await
        .map_err(api)?;
    secrets
        .set("VAULT_A".into(), json!({"token": "tok-2"}))
        .await
        .map_err(api)?;
    assert_eq!(
        secrets.get("VAULT_A".into()).await.map_err(api)?,
        Some(json!({"token": "tok-2"}))
    );
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT ciphertext FROM secret_values WHERE name = 'VAULT_A'")
            .fetch_one(&pool)
            .await?;
    assert!(!stored.windows(5).any(|window| window == b"tok-2"));
    // A row copied under another name does not read as that name.
    sqlx::query("INSERT INTO secret_values (name, ciphertext) VALUES ('VAULT_B', ?)")
        .bind(&stored)
        .execute(&pool)
        .await?;
    assert!(secrets.get("VAULT_B".into()).await.is_err());
    secrets
        .delete(vec!["VAULT_A".into(), "VAULT_B".into()])
        .await
        .map_err(api)?;
    assert_eq!(secrets.get("VAULT_A".into()).await.map_err(api)?, None);

    // The same passphrase unlocks the key again; another one is refused.
    secrets
        .set("VAULT_C".into(), json!("kept"))
        .await
        .map_err(api)?;
    let again = Secrets::default();
    assert_eq!(
        again
            .configure(
                directory.path().to_path_buf(),
                pool.clone(),
                PASSPHRASE.into()
            )
            .await?,
        vec!["VAULT_C".to_owned()]
    );
    assert_eq!(
        again.get("VAULT_C".into()).await.map_err(api)?,
        Some(json!("kept"))
    );
    let refused = Secrets::default()
        .configure(
            directory.path().to_path_buf(),
            pool,
            "another-passphrase-0123456789-abcdefgh".into(),
        )
        .await
        .err()
        .context("another passphrase was accepted")?;
    assert_eq!(
        refused.to_string(),
        format!(
            "the vault passphrase cannot read the secrets stored in {}",
            directory.path().join("secrets").display()
        )
    );
    Ok(())
}

#[tokio::test]
async fn the_legacy_file_moves_into_rows_and_is_removed() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let legacy =
        codex_secrets::LocalSecretsBackend::new(directory.path().to_path_buf(), Arc::new(Legacy));
    for (name, value) in [
        ("VAULT_OLD", json!({"token": "old-token"})),
        ("SESSION_ENV_ABC", json!({"srv": {"K": "v"}})),
    ] {
        legacy.set(
            &codex_secrets::SecretScope::Global,
            &codex_secrets::SecretName::new(name)?,
            &value.to_string(),
        )?;
    }
    let file = directory.path().join("secrets").join(LEGACY_FILE);
    assert!(file.exists());
    let pool = pool(directory.path()).await?;

    // A wrong passphrase cannot read the legacy file either, and leaves it.
    assert!(
        Secrets::default()
            .configure(
                directory.path().to_path_buf(),
                pool.clone(),
                "another-passphrase-0123456789-abcdefgh".into()
            )
            .await
            .is_err()
    );
    assert!(file.exists());

    let secrets = Secrets::default();
    let mut stored = secrets
        .configure(directory.path().to_path_buf(), pool, PASSPHRASE.into())
        .await?;
    stored.sort();
    assert_eq!(
        stored,
        vec!["SESSION_ENV_ABC".to_owned(), "VAULT_OLD".to_owned()]
    );
    assert_eq!(
        secrets.get("VAULT_OLD".into()).await.map_err(api)?,
        Some(json!({"token": "old-token"}))
    );
    assert_eq!(
        secrets.get("SESSION_ENV_ABC".into()).await.map_err(api)?,
        Some(json!({"srv": {"K": "v"}}))
    );
    assert!(!file.exists());
    Ok(())
}
