use super::*;
use codex_utils_cargo_bin::find_resource;
use pretty_assertions::assert_eq;

/// Whether a body is the public error shape the SDK parses.
fn public_error(body: &Value) -> bool {
    let error = &body["error"];
    error["message"]
        .as_str()
        .is_some_and(|message| !message.is_empty())
        && error["type"].is_string()
        && error.get("param").is_some()
        && error.get("code").is_some()
}

#[tokio::test]
async fn every_inventoried_operation_answers_with_the_public_error_shape() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 60), async {
        let inventory: Value = serde_json::from_str(&std::fs::read_to_string(find_resource!("CONTRACT_INVENTORY.json")?)?)?;
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let provider = create_mock_responses_server_repeating_assistant("unused").await;
        MockResponsesConfig::new(&provider.uri()).write(home.path())?;
        let api = AgentsApi::new(backend(home.path()).await?, AbsolutePathBuf::from_absolute_path(data.path())?, TOKEN.into()).await?;
        let (base, _server) = capabilities::serve(&api).await?;
        let client = reqwest::Client::new();
        let mut observed = Vec::new();
        for operation in inventory["operations"].as_array().context("operations")? {
            if operation["status"] == "missing" {
                continue;
            }
            let method: reqwest::Method = operation["method"].as_str().context("method")?.parse()?;
            let path = operation["path"].as_str().context("path")?;
            let path = path.split('/').map(|segment| if segment.starts_with('{') { "missing-id" } else { segment }).collect::<Vec<_>>().join("/");
            let url = format!("{base}{path}");
            let send = |authorized: bool, beta: bool| {
                let mut builder = client.request(method.clone(), &url);
                if beta {
                    builder = builder.header("OpenAI-Beta", "agents=v1");
                }
                if authorized {
                    builder = builder.bearer_auth(TOKEN);
                }
                if method == reqwest::Method::POST {
                    builder = builder.json(&json!({}));
                }
                builder.send()
            };
            for beta in [true, false] {
                for authorized in [false, true] {
                    let response = send(authorized, beta).await?;
                    let status = response.status().as_u16();
                    let body: Value = response.json().await.unwrap_or(Value::Null);
                    observed.push(json!({"id":operation["id"],"authorized":authorized,"status":status,"public":public_error(&body)}));
                }
            }
        }
        // Every operation refuses a missing token, then answers placeholder IDs
        // with 404 and empty bodies with 400; lists succeed. Errors always use
        // the public shape; successes never do. The `OpenAI-Beta` header
        // changes nothing, so each operation answers the same without it.
        let created_from_empty_body = ["VLT-001"];
        // POSTs that validate the body before looking up the resource.
        let validated_first = ["EVT-001", "ENV-002", "CRD-001", "CRD-003", "WHK-007"];
        let expected: Vec<Value> = inventory["operations"].as_array().context("operations")?.iter()
            .filter(|operation| operation["status"] != "missing")
            .flat_map(|operation| {
                let id = operation["id"].as_str().unwrap_or_default();
                let named = operation["path"].as_str().unwrap_or_default().contains('{');
                let status = match (operation["method"].as_str().unwrap_or_default(), named) {
                    ("GET", false) => 200,
                    ("POST", false) if created_from_empty_body.contains(&id) => 200,
                    ("POST", false) => 400,
                    ("POST", true) if validated_first.contains(&id) => 400,
                    _ => 404,
                };
                let answers = [json!({"id":id,"authorized":false,"status":401,"public":true}),
                    json!({"id":id,"authorized":true,"status":status,"public":status != 200})];
                [answers.clone(), answers].concat()
            })
            .collect();
        assert_eq!(observed, expected);
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn every_list_bounds_its_pagination() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 60), async {
        let inventory: Value = serde_json::from_str(&std::fs::read_to_string(find_resource!("CONTRACT_INVENTORY.json")?)?)?;
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let provider = create_mock_responses_server_repeating_assistant("unused").await;
        MockResponsesConfig::new(&provider.uri()).write(home.path())?;
        let api = AgentsApi::new(backend(home.path()).await?, AbsolutePathBuf::from_absolute_path(data.path())?, TOKEN.into()).await?;
        let (base, _server) = capabilities::serve(&api).await?;
        api.configure_environments("an-environment-key-for-pagination-checks".into(), format!("{}/registry", base.trim_end_matches("/v1")))?;
        let client = reqwest::Client::new();
        // Parents that need no model call: an idle self-hosted session, a vault,
        // and a skill.
        let session = request(&client, reqwest::Method::POST, &format!("{base}/agents/sessions"), json!({"agent":{"model":"mock-model"},
            "environment":{"type":"self_hosted","workspace_directory":workspace.path().to_str().context("workspace")?}})).await?;
        let vault: Value = client.post(format!("{base}/vaults")).bearer_auth(TOKEN).header("OpenAI-Beta", "agents=v1")
            .json(&json!({"name":"pages"})).send().await?.error_for_status()?.json().await?;
        let skill = skills::create(&client, &base, TOKEN, "pages").await?;
        let parent = |name: &str| match name {
            "{skill_id}" => skill["id"].as_str().map(str::to_owned),
            "{session_id}" => session["id"].as_str().map(str::to_owned),
            "{vault_id}" => vault["id"].as_str().map(str::to_owned),
            "{environment_id}" => session["environment"]["id"].as_str().map(str::to_owned),
            _ => Some("missing-id".to_owned()),
        };
        let queries = ["", "?limit=0", "?limit=101", "?order=sideways", "?after=missing-id"];
        let mut observed = Vec::new();
        for operation in inventory["operations"].as_array().context("operations")? {
            let path = operation["path"].as_str().unwrap_or_default();
            if operation["status"] == "missing" || operation["method"] != "GET" || operation["sdk_method"] != "list" {
                continue;
            }
            let path = path.split('/').map(|segment| if segment.starts_with('{') { parent(segment).unwrap_or_default() } else { segment.to_owned() }).collect::<Vec<_>>().join("/");
            for query in queries {
                let response = client.get(format!("{base}{path}{query}")).bearer_auth(TOKEN).header("OpenAI-Beta", "agents=v1").send().await?;
                let status = response.status().as_u16();
                let body: Value = response.json().await.unwrap_or(Value::Null);
                observed.push(json!({"id":operation["id"],"query":query,"status":status,"public":public_error(&body)}));
            }
        }
        // Lists reject out-of-range limits, unknown orders, and unknown
        // cursors, except where the contract says otherwise: vault,
        // credential, and webhook endpoint lists clamp `limit` to 1..=100,
        // file lists allow up to 10,000, and webhook event types take no
        // parameters. Subagent lists answer 404 for the placeholder subagent,
        // and environment files need a connected executor (409).
        let expected: Vec<Value> = observed.iter().map(|row| {
            let id = row["id"].as_str().unwrap_or_default();
            let query = row["query"].as_str().unwrap_or_default();
            let status = match (id, query) {
                ("SUB-003" | "SUB-005" | "SUB-006", _) => 404,
                ("ENV-003", "") => 409,
                ("WHK-008", _) | (_, "") => 200,
                ("VLT-003" | "CRD-004" | "WHK-004", "?limit=0" | "?limit=101") => 200,
                ("FIL-003", "?limit=101") => 200,
                _ => 400,
            };
            json!({"id":id,"query":query,"status":status,"public":status != 200})
        }).collect();
        assert_eq!(observed, expected);
        assert_eq!(observed.len(), 17 * queries.len());
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

/// The IDs a list returns when walked one item at a time, after checking each
/// page's `first_id`, `last_id`, and `has_more` and that the walk matches a
/// single full page.
async fn walk(
    client: &reqwest::Client,
    url: &str,
    order: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let order = order
        .map(|order| format!("&order={order}"))
        .unwrap_or_default();
    let get = |query: String| async move {
        anyhow::Ok(
            client
                .get(format!("{url}?{query}"))
                .bearer_auth(TOKEN)
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await?,
        )
    };
    let mut walked = Vec::new();
    let mut after = String::new();
    loop {
        let page = get(format!("limit=1{order}{after}")).await?;
        let id = page["data"][0]["id"]
            .as_str()
            .context("page item")?
            .to_owned();
        assert_eq!(
            (
                page["data"].as_array().map(Vec::len),
                &page["first_id"],
                &page["last_id"]
            ),
            (Some(1), &json!(id), &json!(id)),
            "{url}"
        );
        after = format!("&after={id}");
        walked.push(id);
        if page["has_more"] == false {
            break;
        }
    }
    let full = get(format!("limit=100{order}")).await?;
    assert_eq!(
        (
            full["data"].as_array().map(|data| data
                .iter()
                .map(|item| item["id"].clone())
                .collect::<Vec<_>>()),
            &full["has_more"]
        ),
        (
            Some(walked.iter().map(|id| json!(id)).collect()),
            &json!(false)
        ),
        "{url}"
    );
    Ok(walked)
}

#[tokio::test]
async fn every_list_walks_its_pages_in_both_orders() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 60), async {
        let inventory: Value = serde_json::from_str(&std::fs::read_to_string(find_resource!("CONTRACT_INVENTORY.json")?)?)?;
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let provider = create_mock_responses_server_repeating_assistant("unused").await;
        MockResponsesConfig::new(&provider.uri()).write(home.path())?;
        let api = AgentsApi::new(backend(home.path()).await?, AbsolutePathBuf::from_absolute_path(data.path())?, TOKEN.into()).await?;
        api.configure_vault("operator-pagination-passphrase-for-tests-0123456789".into()).await?;
        api.allow_webhook_hosts(["127.0.0.1".to_string()]);
        let (base, _server) = capabilities::serve(&api).await?;
        let client = reqwest::Client::new();
        let post = |path: String, body: Value| {
            let (client, url) = (&client, format!("{base}{path}"));
            async move { request(client, reqwest::Method::POST, &url, body).await }
        };
        let id = |created: Value| created["id"].as_str().map(str::to_owned).context("created id");

        // Three of each listed resource, in creation order.
        let (mut agents, mut sessions, mut vaults, mut credentials, mut files, mut skills, mut versions, mut endpoints) =
            (vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![]);
        for n in 0..3 {
            agents.push(id(post("/agents".into(), json!({"model":"mock-model"})).await?)?);
            sessions.push(id(post("/agents/sessions".into(), json!({"agent":{"model":"mock-model"},"environment":{"type":"none"},"input":"hello"})).await?)?);
            vaults.push(id(post("/vaults".into(), json!({"name":format!("vault-{n}")})).await?)?);
            credentials.push(id(post(format!("/vaults/{}/credentials", vaults[0]), json!({"name":format!("credential-{n}"),
                "auth":{"type":"static_bearer","token":"secret","mcp_server_url":format!("https://mcp{n}.example.com/mcp")}})).await?)?);
            files.push(id(files::upload(&client, &base, &[("purpose", "user_data")], "page.txt", b"page").await?.error_for_status()?.json().await?)?);
            skills.push(id(skills::create(&client, &base, TOKEN, &format!("page-{n}")).await?)?);
            let archive = skills::skill_zip("page-0", "A skill for tests. Use when testing.", &[("extra.md", format!("version {n}").as_bytes())])?;
            versions.push(id(skills::upload(&client, &format!("{base}/skills/{}/versions", skills[0]), TOKEN, &[], &[("page-0.zip", &archive)])
                .await?.error_for_status()?.json().await?)?);
            endpoints.push(id(post("/webhook_endpoints".into(), json!({"name":format!("hook-{n}"),"url":"http://127.0.0.1:9/hook","event_types":["agent.session.idle"]})).await?)?);
        }

        // Ascending walks list the created resources in creation order (skill
        // versions in version order, after the skill's first version), and
        // descending walks reverse them. Webhook endpoints list newest first
        // and take no order.
        let lists = [
            ("AGT-004", "/agents".to_owned(), &agents),
            ("SES-004", "/agents/sessions".to_owned(), &sessions),
            ("VLT-003", "/vaults".to_owned(), &vaults),
            ("CRD-004", format!("/vaults/{}/credentials", vaults[0]), &credentials),
            ("FIL-003", "/files".to_owned(), &files),
            ("SKL-004", "/skills".to_owned(), &skills),
            ("SKV-003", format!("/skills/{}/versions", skills[0]), &versions),
        ];
        for (operation, path, created) in &lists {
            let url = format!("{base}{path}");
            let ascending = walk(&client, &url, Some("asc")).await?;
            let mut descending = walk(&client, &url, Some("desc")).await?;
            assert_eq!(walk(&client, &url, None).await?, descending, "{operation} defaults to desc");
            descending.reverse();
            assert_eq!(ascending, descending, "{operation}");
            let skipped = ascending.len() - created.len();
            assert_eq!((skipped, &ascending[skipped..]), (usize::from(*operation == "SKV-003"), created.as_slice()), "{operation}");
        }
        let mut newest_first = endpoints.clone();
        newest_first.reverse();
        assert_eq!(walk(&client, &format!("{base}/webhook_endpoints"), None).await?, newest_first);

        // Every other list is walked where its items are produced.
        let elsewhere = [
            "ITM-001", "TRN-002", // contract::public_sessions_save_normalized_history_and_validate_inputs
            "SUB-002", "SUB-003", "SUB-005", "SUB-006", // subagents::*
            "ENV-003", // opaque page tokens: environments::environment_files_are_written_and_listed_inside_the_workspace
            "ART-002", // always empty: artifacts need OpenAI-hosted environments
            "WHK-008", // not paginated
        ];
        let walked: Vec<&str> = lists.iter().map(|(operation, _, _)| *operation).chain(["WHK-004"]).chain(elsewhere).collect();
        let mut inventoried: Vec<&str> = inventory["operations"].as_array().context("operations")?.iter()
            .filter(|operation| operation["status"] != "missing" && operation["sdk_method"] == "list")
            .filter_map(|operation| operation["id"].as_str()).collect();
        inventoried.sort_unstable();
        let mut walked = walked;
        walked.sort_unstable();
        assert_eq!(walked, inventoried);
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

/// Builds an operation's request body around some metadata.
type BodyFor<'a> = Box<dyn Fn(&Value) -> Value + 'a>;

#[tokio::test]
async fn metadata_limits_hold_on_every_resource_that_takes_metadata() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 60), async {
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let provider = create_mock_responses_server_repeating_assistant("unused").await;
        MockResponsesConfig::new(&provider.uri()).write(home.path())?;
        let api = AgentsApi::new(backend(home.path()).await?, AbsolutePathBuf::from_absolute_path(data.path())?, TOKEN.into()).await?;
        let (base, _server) = capabilities::serve(&api).await?;
        api.configure_environments("an-environment-key-for-metadata-checks".into(), format!("{}/registry", base.trim_end_matches("/v1")))?;
        let client = reqwest::Client::new();
        let send = |method: reqwest::Method, url: String, body: Value| {
            client.request(method, url).bearer_auth(TOKEN).header("OpenAI-Beta", "agents=v1").json(&body).send()
        };
        // Limits count characters, so multi-byte text sits exactly at them.
        let pairs = |count: usize| (0..count).map(|index| (format!("k{index}"), json!("v"))).collect::<serde_json::Map<_, _>>();
        let accepted = [
            Value::Object(pairs(16)),
            json!({"é".repeat(64): "ü".repeat(512)}),
        ];
        let refused = [
            Value::Object(pairs(17)),
            json!({"é".repeat(65): "v"}),
            json!({"k": "ü".repeat(513)}),
            json!({"k": 1}),
        ];
        let workspace_directory = workspace.path().to_str().context("workspace")?;
        let agent: Value = send(reqwest::Method::POST, format!("{base}/agents"), json!({"model":"mock-model"})).await?.json().await?;
        let session: Value = send(reqwest::Method::POST, format!("{base}/agents/sessions"), json!({"agent":{"model":"mock-model"},
            "environment":{"type":"self_hosted","workspace_directory":workspace_directory}})).await?.json().await?;
        let agent_url = format!("{base}/agents/{}", agent["id"].as_str().context("agent id")?);
        let session_url = format!("{base}/agents/sessions/{}", session["id"].as_str().context("session id")?);
        let operations: [(&str, String, BodyFor<'_>); 5] = [
            ("agent create", format!("{base}/agents"), Box::new(|metadata: &Value| json!({"model":"mock-model","metadata":metadata}))),
            ("agent update", agent_url, Box::new(|metadata: &Value| json!({"metadata":metadata}))),
            ("session create", format!("{base}/agents/sessions"), Box::new(move |metadata: &Value| json!({"agent":{"model":"mock-model"},
                "environment":{"type":"self_hosted","workspace_directory":workspace_directory},"metadata":metadata}))),
            ("session update", session_url, Box::new(|metadata: &Value| json!({"metadata":metadata}))),
            ("vault create", format!("{base}/vaults"), Box::new(|metadata: &Value| json!({"name":"bounds","metadata":metadata}))),
        ];
        let mut observed = Vec::new();
        let mut expected = Vec::new();
        for (name, url, body) in &operations {
            for (metadata, status) in accepted.iter().map(|metadata| (metadata, 200)).chain(refused.iter().map(|metadata| (metadata, 400))) {
                let response = send(reqwest::Method::POST, url.clone(), body(metadata)).await?;
                let code = response.status().as_u16();
                let reply: Value = response.json().await?;
                let saved = if code == 200 { reply["metadata"].clone() } else { reply["error"]["message"].clone() };
                observed.push(json!({"operation":name,"status":code,"result":saved}));
                expected.push(json!({"operation":name,"status":status,
                    "result":if status == 200 { metadata.clone() } else if metadata["k"] == 1 { json!("metadata must contain string pairs") } else { json!("metadata exceeds its size limit") }}));
            }
        }
        assert_eq!(observed, expected);
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
