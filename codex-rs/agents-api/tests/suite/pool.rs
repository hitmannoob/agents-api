//! Run mode's worker pool (RUN_MODE.md): sessions are spread over workers,
//! each session's thread lives in its own worker's home, and losing one
//! worker leaves the other workers' sessions and streams alone.
use super::sessions::rollouts;
use super::*;
use pretty_assertions::assert_eq;

/// Wait until the session has `count` turns, all completed.
async fn turns_completed(client: &reqwest::Client, url: &str, count: usize) -> anyhow::Result<()> {
    loop {
        let turns = request(
            client,
            reqwest::Method::GET,
            &format!("{url}/turns"),
            Value::Null,
        )
        .await?;
        let data = turns["data"].as_array().context("turns")?;
        if data.len() == count && data.iter().all(|turn| turn["status"] == "completed") {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await;
    }
}

async fn start(client: &reqwest::Client, base: &str, input: &str) -> anyhow::Result<String> {
    let session = request(
        client,
        reqwest::Method::POST,
        &format!("{base}/agents/sessions"),
        json!({"agent":{"model":"mock-model"},"environment":{"type":"none"},"input":input}),
    )
    .await?;
    let url = format!(
        "{base}/agents/sessions/{}",
        session["id"].as_str().context("session id")?
    );
    turns_completed(client, &url, /*count*/ 1).await?;
    Ok(url)
}

async fn follow_up(client: &reqwest::Client, url: &str, input: &str) -> anyhow::Result<()> {
    // Accepted input answers 202 with no body.
    client
        .post(format!("{url}/events"))
        .bearer_auth(TOKEN)
        .json(&json!({"events":[{"type":"agent.session.input.message","input":input}]}))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

#[tokio::test]
async fn pool_gives_sessions_their_own_workers_and_isolates_a_lost_worker() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 120), async {
        let homes = [tempfile::tempdir()?, tempfile::tempdir()?];
        let data = tempfile::tempdir()?;
        let model = create_mock_responses_server_repeating_assistant("finished").await;
        for home in &homes {
            MockResponsesConfig::new(&model.uri())
                .with_root_config("features.plugins = false")
                .write(home.path())?;
        }
        let api = AgentsApi::with_workers(
            vec![
                backend(homes[0].path()).await?,
                backend(homes[1].path()).await?,
            ],
            AbsolutePathBuf::from_absolute_path(data.path())?,
            TOKEN.into(),
        )
        .await?;
        assert_eq!(api.workers(), 2);
        let (base, server) = capabilities::serve(&api).await?;
        let root = base.strip_suffix("/v1").context("API root")?;
        let client = reqwest::Client::new();
        let threads = |worker: usize| match homes[worker].path().join("sessions") {
            dir if dir.exists() => rollouts(&dir),
            _ => Ok(0),
        };

        // Each of two sessions gets a worker of its own, and its thread lives
        // in that worker's home.
        let first = start(&client, &base, "Remember alpha-101").await?;
        assert_eq!((threads(0)?, threads(1)?), (1, 0));
        let second = start(&client, &base, "Remember beta-202").await?;
        assert_eq!((threads(0)?, threads(1)?), (1, 1));
        let health: Value = client
            .get(format!("{root}/healthz"))
            .send()
            .await?
            .json()
            .await?;
        assert_eq!(
            health,
            json!({"status":"ok","worker":"connected","workers":[
                {"index":0,"status":"connected"},{"index":1,"status":"connected"}]})
        );

        // Losing worker 1 ends only its session's live stream.
        let mut first_stream = client
            .get(format!("{first}/events"))
            .bearer_auth(TOKEN)
            .send()
            .await?
            .error_for_status()?;
        let mut second_stream = client
            .get(format!("{second}/events"))
            .bearer_auth(TOKEN)
            .send()
            .await?
            .error_for_status()?;
        api.reconnect_worker(1, backend(homes[1].path()).await?)
            .await?;
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
            while second_stream.chunk().await?.is_some() {}
            anyhow::Ok(())
        })
        .await??;
        follow_up(&client, &first, "What did I ask you to remember?").await?;
        event(&mut first_stream, "agent.session.idle").await?;
        turns_completed(&client, &first, /*count*/ 2).await?;

        // The replacement worker resumes its session's thread from its home.
        follow_up(&client, &second, "What did I ask you to remember?").await?;
        turns_completed(&client, &second, /*count*/ 2).await?;
        assert_eq!((threads(0)?, threads(1)?), (1, 1));
        let requests = model.received_requests().await.context("model requests")?;
        let last: Value = serde_json::from_slice(&requests.last().context("last request")?.body)?;
        assert!(last["input"].to_string().contains("beta-202"));

        // A deleted session's thread is deleted on its own worker, and the
        // session stops counting towards that worker's load.
        request(&client, reqwest::Method::DELETE, &second, Value::Null).await?;
        while threads(1)? != 0 {
            tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await;
        }
        assert_eq!(threads(0)?, 1);
        start(&client, &base, "Remember gamma-303").await?;
        assert_eq!((threads(0)?, threads(1)?), (1, 1));
        server.abort();
        api.shutdown().await?;
        anyhow::Ok(())
    })
    .await?
}

#[tokio::test]
async fn pool_fills_the_least_loaded_worker() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 120), async {
        let homes = [tempfile::tempdir()?, tempfile::tempdir()?];
        let data = tempfile::tempdir()?;
        let model = create_mock_responses_server_repeating_assistant("finished").await;
        for home in &homes {
            MockResponsesConfig::new(&model.uri())
                .with_root_config("features.plugins = false")
                .write(home.path())?;
        }
        let api = AgentsApi::with_workers(
            vec![
                backend(homes[0].path()).await?,
                backend(homes[1].path()).await?,
            ],
            AbsolutePathBuf::from_absolute_path(data.path())?,
            TOKEN.into(),
        )
        .await?;
        let (base, server) = capabilities::serve(&api).await?;
        let client = reqwest::Client::new();
        // Sessions started together still spread evenly.
        let started = futures::future::join_all((0..4).map(|index| {
            let client = client.clone();
            let base = base.clone();
            async move { start(&client, &base, &format!("Session {index}")).await }
        }))
        .await;
        for result in started {
            result?;
        }
        let threads = |worker: usize| match homes[worker].path().join("sessions") {
            dir if dir.exists() => rollouts(&dir),
            _ => Ok(0),
        };
        assert_eq!((threads(0)?, threads(1)?), (2, 2));
        server.abort();
        api.shutdown().await?;
        anyhow::Ok(())
    })
    .await?
}
