use super::*;
use app_test_support::MockResponsesConfig;
use app_test_support::create_mock_responses_server_repeating_assistant;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::Command;

const TOKEN: &str = "test-token-for-managed-agents-api";

async fn start_api(
    data: &Path,
    socket: Option<&AbsolutePathBuf>,
) -> anyhow::Result<(Child, String, Option<u32>)> {
    let mut command = Command::new(cargo_bin("codex-agents-api")?);
    command
        .args(["--listen", "127.0.0.1:0", "--data-directory"])
        .arg(data)
        .env("CODEX_AGENTS_API_TOKEN", TOKEN)
        .env("CODEX_APP_SERVER_DISABLE_MANAGED_CONFIG", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match socket {
        Some(socket) => {
            command.arg("--app-server-socket").arg(socket.as_path());
        }
        None => {
            command
                .arg("--app-server-bin")
                .arg(cargo_bin("codex-app-server")?);
        }
    }
    let mut child = command.spawn()?;
    let mut lines = BufReader::new(child.stderr.take().context("stderr")?).lines();
    let mut worker_pid = None;
    let base = tokio::time::timeout(DEADLINE, async {
        while let Some(line) = lines.next_line().await? {
            if let Some(pid) = line.strip_prefix("agents-api managed worker pid=") {
                worker_pid = Some(pid.parse()?);
            }
            if let Some(address) = line.strip_prefix("agents-api listening on ") {
                return anyhow::Ok(format!("http://{address}/v1"));
            }
            eprintln!("{line}");
        }
        anyhow::bail!("API exited before readiness: {}", child.wait().await?);
    })
    .await??;
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("{line}");
        }
    });
    Ok((child, base, worker_pid))
}

async fn stop_api(mut child: Child) -> anyhow::Result<()> {
    let pid = i32::try_from(child.id().context("API process ID")?)?;
    // SAFETY: this PID belongs to our live, unreaped child process.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let status = tokio::time::timeout(DEADLINE, child.wait()).await??;
    assert!(status.success(), "API shutdown failed: {status}");
    Ok(())
}

async fn turns(client: &reqwest::Client, url: &str, count: usize) -> anyhow::Result<Value> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let response: Value = client
                .get(format!("{url}/turns"))
                .bearer_auth(TOKEN)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let data = response["data"].as_array().context("turns")?;
            if data.len() == count && data.iter().all(|turn| turn["status"] == "completed") {
                return anyhow::Ok(response);
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await;
        }
    })
    .await?
}

#[tokio::test]
async fn invalid_public_url_is_rejected_at_startup() -> anyhow::Result<()> {
    for from_env in [false, true] {
        let data = tempfile::tempdir()?;
        let mut command = Command::new(cargo_bin("codex-agents-api")?);
        command
            .args(["--listen", "127.0.0.1:0", "--data-directory"])
            .arg(data.path())
            .arg("--app-server-bin")
            .arg(cargo_bin("codex-app-server")?)
            .env("CODEX_AGENTS_API_TOKEN", TOKEN)
            .env("CODEX_APP_SERVER_DISABLE_MANAGED_CONFIG", "1")
            .kill_on_drop(true);
        if from_env {
            command.env(
                "CODEX_AGENTS_API_PUBLIC_URL",
                "https://public.example/registry",
            );
        } else {
            command
                .env_remove("CODEX_AGENTS_API_PUBLIC_URL")
                .args(["--public-url", "https://public.example/registry"]);
        }
        let output = tokio::time::timeout(DEADLINE, command.output()).await??;
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr)?;
        assert!(stderr.contains("public URL must be "), "{stderr}");
        assert!(!stderr.contains("agents-api listening on "), "{stderr}");
    }
    Ok(())
}

#[tokio::test]
async fn cli_manages_worker_and_resumes_saved_session_after_restart() -> anyhow::Result<()> {
    let data = tempfile::tempdir()?;
    let home = data.path().join("codex-home");
    std::fs::create_dir_all(&home)?;
    let model = create_mock_responses_server_repeating_assistant("finished").await;
    MockResponsesConfig::new(&model.uri())
        .with_root_config("features.plugins = false")
        .write(&home)?;
    let client = reqwest::Client::new();
    let (api, base, _) = start_api(data.path(), /*socket*/ None).await?;
    let session: Value = client.post(format!("{base}/agents/sessions")).bearer_auth(TOKEN)
        .json(&json!({"agent":{"model":"mock-model"},"environment":{"type":"none"},"input":"Remember orange-731"}))
        .send().await?.error_for_status()?.json().await?;
    let id = session["id"].as_str().context("session id")?;
    let saved = turns(
        &client,
        &format!("{base}/agents/sessions/{id}"),
        /*count*/ 1,
    )
    .await?;
    stop_api(api).await?;
    let (api, base, _) = start_api(data.path(), /*socket*/ None).await?;
    let url = format!("{base}/agents/sessions/{id}");
    assert_eq!(turns(&client, &url, /*count*/ 1).await?, saved);
    client.post(format!("{url}/events")).bearer_auth(TOKEN)
        .json(&json!({"events":[{"type":"agent.session.input.message", "input":"What code did I ask you to remember?"}]}))
        .send().await?.error_for_status()?;
    turns(&client, &url, /*count*/ 2).await?;
    stop_api(api).await?;
    let requests = model.received_requests().await.context("model requests")?;
    let last: Value = serde_json::from_slice(&requests.last().context("last request")?.body)?;
    assert!(last["input"].to_string().contains("orange-731"));
    Ok(())
}

#[tokio::test]
async fn api_shutdown_preserves_external_worker() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let data = tempfile::tempdir()?;
    let owned = worker(home.path()).await?;
    let (api, _, _) = start_api(data.path(), Some(owned.socket())).await?;
    stop_api(api).await?;
    // A full initialization still succeeds after the API has exited.
    runtime::connect(owned.socket().clone(), DEADLINE)
        .await?
        .shutdown()
        .await?;
    owned.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn managed_worker_restarts_after_crash_and_service_continues() -> anyhow::Result<()> {
    let data = tempfile::tempdir()?;
    let home = data.path().join("codex-home");
    std::fs::create_dir_all(&home)?;
    let model = create_mock_responses_server_repeating_assistant("finished").await;
    MockResponsesConfig::new(&model.uri())
        .with_root_config("features.plugins = false")
        .write(&home)?;
    let client = reqwest::Client::new();
    let (mut api, base, pid) = start_api(data.path(), /*socket*/ None).await?;
    let pid = i32::try_from(pid.context("managed worker PID")?)?;
    let session: Value = client
        .post(format!("{base}/agents/sessions"))
        .bearer_auth(TOKEN)
        .json(&json!({"agent":{"model":"mock-model"},"environment":{"type":"none"},"input":"Remember orange-731"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let id = session["id"].as_str().context("session id")?;
    let url = format!("{base}/agents/sessions/{id}");
    turns(&client, &url, /*count*/ 1).await?;
    // SAFETY: the API owns this live worker and has not reaped it.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    // The supervisor respawns the worker; mutations return 503 until it
    // reattaches, then the saved session keeps serving on the replacement.
    tokio::time::timeout(DEADLINE, async {
        loop {
            let response = client
                .post(format!("{url}/events"))
                .bearer_auth(TOKEN)
                .json(&json!({"events":[{"type":"agent.session.input.message","input":"What code did I ask you to remember?"}]}))
                .send()
                .await?;
            if response.status().is_success() {
                return anyhow::Ok(());
            }
            assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
            tokio::time::sleep(Duration::from_millis(/*millis*/ 50)).await;
        }
    })
    .await??;
    turns(&client, &url, /*count*/ 2).await?;
    assert!(
        api.try_wait()?.is_none(),
        "the API must survive a managed worker crash"
    );
    stop_api(api).await?;
    let requests = model.received_requests().await.context("model requests")?;
    let last: Value = serde_json::from_slice(&requests.last().context("last request")?.body)?;
    assert!(last["input"].to_string().contains("orange-731"));
    Ok(())
}

/// Answers model requests, except that the ones numbered in `stall` (from 1)
/// never answer, so their turn is still running when the worker is killed.
struct StallSome {
    seen: std::sync::atomic::AtomicUsize,
    stall: &'static [usize],
    body: String,
}

impl wiremock::Respond for StallSome {
    fn respond(&self, _: &wiremock::Request) -> wiremock::ResponseTemplate {
        let number = self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let answer = wiremock::ResponseTemplate::new(200)
            .set_body_raw(self.body.clone(), "text/event-stream");
        if self.stall.contains(&number) {
            answer.set_delay(Duration::from_secs(/*secs*/ 600))
        } else {
            answer
        }
    }
}

/// The API's current managed worker, once it is serving model requests.
fn current_worker(api: &Child) -> anyhow::Result<i32> {
    let api_pid = api.id().context("API PID")?;
    let output = std::process::Command::new("pgrep")
        .args(["-P", &api_pid.to_string()])
        .output()?;
    let pids: Vec<i32> = String::from_utf8(output.stdout)?
        .split_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect();
    anyhow::ensure!(pids.len() == 1, "expected one worker, found {pids:?}");
    Ok(pids[0])
}

async fn requests(model: &wiremock::MockServer, count: usize) -> anyhow::Result<Vec<Value>> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let received = model.received_requests().await.context("model requests")?;
            if received.len() >= count {
                return received
                    .iter()
                    .map(|request| Ok(serde_json::from_slice(&request.body)?))
                    .collect();
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await;
        }
    })
    .await?
}

async fn turn_states(client: &reqwest::Client, url: &str) -> anyhow::Result<Vec<(String, Value)>> {
    let response: Value = client
        .get(format!("{url}/turns?order=asc"))
        .bearer_auth(TOKEN)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(response["data"]
        .as_array()
        .context("turns")?
        .iter()
        .map(|turn| {
            (
                turn["status"].as_str().unwrap_or_default().to_owned(),
                turn["error"]["message"].clone(),
            )
        })
        .collect())
}

/// By default (`--continue-interrupted-turns 1`), a turn interrupted by the
/// worker's crash is continued on the replacement worker with the conversation so far;
/// a continuation interrupted in turn is not continued again.
#[tokio::test]
async fn interrupted_turns_continue_on_the_replacement_worker() -> anyhow::Result<()> {
    let data = tempfile::tempdir()?;
    let home = data.path().join("codex-home");
    std::fs::create_dir_all(&home)?;
    let model = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path_regex(".*/responses$"))
        .respond_with(StallSome {
            seen: Default::default(),
            stall: &[1, 3, 4],
            body: app_test_support::create_final_assistant_message_sse_response("done")?,
        })
        .mount(&model)
        .await;
    MockResponsesConfig::new(&model.uri())
        .with_root_config("features.plugins = false")
        .write(&home)?;
    let client = reqwest::Client::new();
    let (api, base, _) = start_api(data.path(), /*socket*/ None).await?;
    let session: Value = client
        .post(format!("{base}/agents/sessions"))
        .bearer_auth(TOKEN)
        .json(&json!({"agent":{"model":"mock-model"},"environment":{"type":"none"},"input":"Index the plum-552 catalog"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let url = format!(
        "{base}/agents/sessions/{}",
        session["id"].as_str().context("session id")?
    );
    let kill = |pid: i32| {
        // SAFETY: the API owns this live worker and has not reaped it.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    };
    let lost = json!("backend connection lost");

    // The first turn is running when its worker dies; the replacement worker
    // continues it, and the model sees the original request and the notice.
    requests(&model, 1).await?;
    kill(current_worker(&api)?);
    let continued = requests(&model, 2).await?;
    let input = continued[1]["input"].to_string();
    assert!(
        input.contains("plum-552") && input.contains("so that turn was interrupted"),
        "{input}"
    );
    tokio::time::timeout(DEADLINE, async {
        while turn_states(&client, &url).await?
            != vec![
                ("failed".to_owned(), lost.clone()),
                ("completed".to_owned(), Value::Null),
            ]
        {
            tokio::time::sleep(Duration::from_millis(/*millis*/ 50)).await;
        }
        anyhow::Ok(())
    })
    .await??;

    // A continuation that is itself interrupted is not continued again.
    let accepted = client
        .post(format!("{url}/events"))
        .bearer_auth(TOKEN)
        .json(&json!({"events":[{"type":"agent.session.input.message","input":"Now export it"}]}))
        .send()
        .await?;
    assert!(accepted.status().is_success(), "{}", accepted.status());
    requests(&model, 3).await?;
    kill(current_worker(&api)?);
    requests(&model, 4).await?;
    kill(current_worker(&api)?);
    let settled = tokio::time::timeout(DEADLINE, async {
        loop {
            let states = turn_states(&client, &url).await?;
            if states.len() == 4
                && states
                    .iter()
                    .all(|(status, _)| status != "in_progress" && status != "queued")
            {
                return anyhow::Ok(states);
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 50)).await;
        }
    })
    .await??;
    assert_eq!(
        settled
            .iter()
            .map(|(status, error)| (status.as_str(), error.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("failed", lost.clone()),
            ("completed", Value::Null),
            ("failed", lost.clone()),
            ("failed", lost.clone())
        ]
    );
    // Give a wrongful third continuation time to show up.
    tokio::time::sleep(Duration::from_secs(/*secs*/ 2)).await;
    assert_eq!(
        model
            .received_requests()
            .await
            .context("model requests")?
            .len(),
        4
    );
    let failed: Value = client
        .get(&url)
        .bearer_auth(TOKEN)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(
        (&failed["status"], &failed["error"]),
        (&json!("failed"), &lost)
    );
    stop_api(api).await?;
    Ok(())
}

#[tokio::test]
async fn orphaned_worker_is_reclaimed_after_api_crash() -> anyhow::Result<()> {
    let data = tempfile::tempdir()?;
    let (mut api, _base, worker1) = start_api(data.path(), /*socket*/ None).await?;
    let worker1 = i32::try_from(worker1.context("first worker PID")?)?;
    // Simulate an API-process crash: SIGKILL skips graceful shutdown and
    // kill_on_drop, so the worker is orphaned against the shared home.
    let api_pid = i32::try_from(api.id().context("API PID")?)?;
    // SAFETY: this PID belongs to our live, unreaped API child process.
    assert_eq!(unsafe { libc::kill(api_pid, libc::SIGKILL) }, 0);
    api.wait().await?;
    // The worker outlives the crashed API.
    // SAFETY: kill(pid, 0) probes existence without delivering a signal.
    assert_eq!(
        unsafe {
            libc::kill(worker1, /*sig*/ 0)
        },
        0
    );
    // A second API on the same data directory reclaims the orphan, then spawns
    // a fresh worker and serves normally.
    let (api, base, worker2) = start_api(data.path(), /*socket*/ None).await?;
    let worker2 = i32::try_from(worker2.context("second worker PID")?)?;
    assert_ne!(worker1, worker2, "a fresh worker must replace the orphan");
    tokio::time::timeout(DEADLINE, async {
        // SAFETY: kill(pid, 0) probes existence without delivering a signal.
        while unsafe {
            libc::kill(worker1, /*sig*/ 0)
        } == 0
        {
            tokio::time::sleep(Duration::from_millis(/*millis*/ 50)).await;
        }
        anyhow::Ok(())
    })
    .await??;
    let client = reqwest::Client::new();
    let agent = client
        .post(format!("{base}/agents"))
        .bearer_auth(TOKEN)
        .json(&json!({"model": "mock-model", "instructions": "x"}))
        .send()
        .await?;
    assert!(agent.status().is_success(), "reclaimed API must serve");
    stop_api(api).await?;
    Ok(())
}

#[tokio::test]
async fn unresponsive_worker_is_forced_down_and_reaped() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let owned = worker(home.path()).await?;
    runtime::connect(owned.socket().clone(), DEADLINE)
        .await?
        .shutdown()
        .await?;
    let pid = i32::try_from(owned.id().context("worker PID")?)?;
    // SAFETY: this PID belongs to the live, unreaped worker.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    tokio::time::timeout(DEADLINE, owned.shutdown()).await??;
    // kill(pid, 0) checks existence without delivering a signal; reaping must be complete.
    assert_eq!(
        unsafe {
            libc::kill(pid, /*sig*/ 0)
        },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    Ok(())
}

#[tokio::test]
async fn startup_timeout_reaps_the_owned_process() -> anyhow::Result<()> {
    let data = tempfile::tempdir()?;
    let output = tokio::time::timeout(
        DEADLINE,
        Command::new(cargo_bin("codex-agents-api")?)
            .args([
                "--listen",
                "127.0.0.1:0",
                "--worker-startup-timeout-secs",
                "0",
                "--data-directory",
            ])
            .arg(data.path())
            .arg("--app-server-bin")
            .arg(cargo_bin("codex-app-server")?)
            .env("CODEX_AGENTS_API_TOKEN", TOKEN)
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("initialization timed out"), "{stderr}");
    let pid: i32 = stderr
        .lines()
        .find_map(|line| line.strip_prefix("agents-api managed worker pid="))
        .context("worker PID")?
        .parse()?;
    // SAFETY: signal zero only checks process existence.
    assert_eq!(
        unsafe {
            libc::kill(pid, /*sig*/ 0)
        },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    Ok(())
}
