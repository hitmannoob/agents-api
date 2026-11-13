use super::*;
use axum::http::Method;
use pretty_assertions::assert_eq;

#[test]
fn operations_require_their_resource_permission() {
    let cases: [(Method, &str, &[&str]); 16] = [
        (Method::GET, "/v1/agents", &["api.agents.read"]),
        (Method::POST, "/v1/agents/agent-1", &["api.agents.write"]),
        (
            Method::GET,
            "/v1/agents/sessions/s/events",
            &["api.agents.read"],
        ),
        (
            Method::POST,
            "/v1/agents/sessions",
            &["api.agents.write", "api.responses.write"],
        ),
        (
            Method::POST,
            "/v1/agents/sessions/s/events",
            &["api.agents.write", "api.responses.write"],
        ),
        (
            Method::DELETE,
            "/v1/agents/sessions/s",
            &["api.agents.write"],
        ),
        (
            Method::POST,
            "/v1/agents/environments/e/files",
            &["api.agents.write"],
        ),
        (
            Method::POST,
            "/v1/sessions/s/input",
            &["api.agents.write", "api.responses.write"],
        ),
        (
            Method::POST,
            "/v1/sessions/s/turns/t/tool-results",
            &["api.agents.write", "api.responses.write"],
        ),
        (
            Method::POST,
            "/v1/sessions/s/turns/t/cancel",
            &["api.agents.write", "api.responses.write"],
        ),
        (
            Method::DELETE,
            "/v1/vaults/v/credentials/c",
            &["api.vaults.write"],
        ),
        (Method::GET, "/v1/files/f/content", &["api.files.read"]),
        (
            Method::POST,
            "/v1/webhook_endpoints/w/test",
            &["api.webhooks.write"],
        ),
        (
            Method::GET,
            "/v1/webhook_event_types",
            &["api.webhooks.read"],
        ),
        // A shared prefix is not a resource match.
        (Method::GET, "/v1/vaultsx", &[]),
        (Method::GET, "/v1/unknown", &[]),
    ];
    for (method, path, expected) in cases {
        assert_eq!(required(&method, path), expected, "{method} {path}");
    }
}

#[tokio::test]
async fn keys_belong_to_projects_and_can_be_revoked() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let admin = Admin::open(AbsolutePathBuf::from_absolute_path(directory.path())?).await?;
    let project = admin.create_project(" Research ").await?;
    assert!(project.id.starts_with("proj_"), "{}", project.id);
    assert_eq!(project.name, "Research");
    assert_eq!(
        admin
            .projects()
            .await?
            .iter()
            .map(|project| project.id.as_str())
            .collect::<Vec<_>>(),
        [DEFAULT_PROJECT, project.id.as_str()]
    );

    let permissions = ["api.vaults.read", "api.agents.read", "api.agents.read"].map(String::from);
    let created = admin.create_key(&project.id, "ci", &permissions).await?;
    assert!(created.value.starts_with("sk-proj-"));
    assert!(
        created
            .key
            .redacted_value
            .ends_with(&created.value[created.value.len() - 4..])
    );
    // Permissions are stored deduplicated, in catalog order.
    assert_eq!(
        created.key.permissions,
        ["api.agents.read", "api.vaults.read"]
    );
    let listed = admin.keys(Some(&project.id)).await?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, created.key.id);
    assert!(admin.keys(Some(DEFAULT_PROJECT)).await?.is_empty());

    let digest_matches: Option<String> =
        sqlx::query_scalar("SELECT project_id FROM api_keys WHERE secret_sha256 = ?")
            .bind(digest(&created.value))
            .fetch_optional(&admin.0)
            .await?;
    assert_eq!(digest_matches, Some(project.id.clone()));

    admin.revoke_key(&created.key.id).await?;
    let revoked_at = admin.keys(None).await?[0].revoked_at;
    assert!(revoked_at.is_some());
    admin.revoke_key(&created.key.id).await?;
    assert_eq!(admin.keys(None).await?[0].revoked_at, revoked_at);

    for (project_id, permissions, error) in [
        (
            "proj_missing",
            vec!["api.agents.read".to_owned()],
            "no project proj_missing",
        ),
        (
            project.id.as_str(),
            vec![],
            "a key needs at least one permission",
        ),
        (
            project.id.as_str(),
            vec!["api.everything".to_owned()],
            "unknown permission api.everything",
        ),
    ] {
        let failure = admin
            .create_key(project_id, "", &permissions)
            .await
            .expect_err("invalid key request");
        assert!(failure.to_string().starts_with(error), "{failure}");
    }
    assert!(admin.revoke_key("key_missing").await.is_err());
    assert!(admin.create_project("  ").await.is_err());
    Ok(())
}

#[test]
fn resource_paths_reject_ambiguous_encodings() {
    let params = |id: &str| std::collections::HashMap::from([("id".to_owned(), id.to_owned())]);
    for (raw, decoded) in [
        ("/v1/files/%66ile-abc", "file-abc"),
        ("/v1/files/fi%6Ce-abc", "file-abc"),
        ("/v1/files/fi%6ce-abc", "file-abc"),
        ("/v1/files/FILE-abc", "FILE-abc"),
    ] {
        assert!(unambiguous_path(raw, &params(decoded)), "{raw}");
    }
    for (raw, decoded) in [
        ("/v1/files/file%", "file%"),
        ("/v1/files/file%6", "file%6"),
        ("/v1/files/file%GG", "file%GG"),
        ("/v1/files/file%2Fabc", "file/abc"),
        ("/v1/files/file%5cabc", "file\\abc"),
        ("/v1/files/%2566ile-abc", "%66ile-abc"),
        ("/v1/files/file%00abc", "file\0abc"),
        ("/v1/files/%2e%2e", ".."),
        ("/v1/files/", ""),
    ] {
        assert!(!unambiguous_path(raw, &params(decoded)), "{raw}");
    }
}
