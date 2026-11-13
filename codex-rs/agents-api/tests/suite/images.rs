use super::*;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use pretty_assertions::assert_eq;
use wiremock::MockServer;
use wiremock::matchers::path;

/// A 1x1 PNG, the same pixel the function result tests send inline.
const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";

fn image_input(url: &str) -> Value {
    json!([{"role":"user","content":[{"type":"input_text","text":"describe-the-image"},{"type":"input_image","image_url":url}]}])
}

// Codex prepares images on a blocking thread.
#[tokio::test(flavor = "multi_thread")]
async fn remote_input_images_are_fetched_under_the_egress_rules() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 60), async {
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let provider = create_mock_responses_server_repeating_assistant("a pixel").await;
        MockResponsesConfig::new(&provider.uri()).with_root_config("features.plugins = false").write(home.path())?;
        let images = MockServer::start().await;
        for (route, response) in [
            ("/pixel.png", ResponseTemplate::new(/*s*/ 200).set_body_raw(STANDARD.decode(PNG)?, "image/png")),
            ("/page.html", ResponseTemplate::new(/*s*/ 200).set_body_raw("<html></html>", "text/html")),
            ("/missing.png", ResponseTemplate::new(/*s*/ 404)),
            ("/moved.png", ResponseTemplate::new(/*s*/ 302).insert_header("location", "/pixel.png")),
            ("/large.png", ResponseTemplate::new(/*s*/ 200).set_body_raw(vec![0u8; 15 << 20], "image/png")),
        ] {
            Mock::given(path(route)).respond_with(response).mount(&images).await;
        }
        let api = AgentsApi::new(backend(home.path()).await?, AbsolutePathBuf::from_absolute_path(data.path())?, TOKEN.into()).await?;
        let (base, server) = capabilities::serve(&api).await?;
        let client = reqwest::Client::new();
        let sessions = format!("{base}/agents/sessions");
        let create = |input: Value| client.post(&sessions).bearer_auth(TOKEN)
            .json(&json!({"agent":{"model":"mock-model"},"environment":{"type":"none"},"input":input})).send();
        let refused = |response: reqwest::Response| async move {
            anyhow::Ok((response.status(), response.json::<Value>().await?["error"]["message"].clone()))
        };
        let pixel = format!("{}/pixel.png", images.uri());

        // Internal hosts need the operator's allowance, and only https or
        // inline data URLs are accepted otherwise.
        assert_eq!(refused(create(image_input(&pixel)).await?).await?, (reqwest::StatusCode::BAD_REQUEST,
            json!("input_image image_url must be a data:image URL or an absolute https URL")));
        assert_eq!(refused(create(image_input("https://127.0.0.1:9/pixel.png")).await?).await?, (reqwest::StatusCode::BAD_REQUEST,
            json!("input_image image_url resolves to a non-public address; the operator must allow its host")));
        assert_eq!(refused(create(image_input("ftp://example.com/pixel.png")).await?).await?, (reqwest::StatusCode::BAD_REQUEST,
            json!("input_image requires a data:image URL or an absolute https URL")));
        api.allow_image_hosts(["127.0.0.1".to_string()]);

        // An allowed image reaches the model inline.
        let session = request(&client, reqwest::Method::POST, &sessions,
            json!({"agent":{"model":"mock-model"},"environment":{"type":"none"},"input":image_input(&pixel)})).await?;
        let url = format!("{sessions}/{}", session["id"].as_str().context("session id")?);
        idle(&client, &url, /*expected_turns*/ 1).await?;
        let captures = provider.received_requests().await.context("captures")?;
        let sent = captures.iter().filter(|r| r.url.path().ends_with("/responses"))
            .find(|r| String::from_utf8_lossy(&r.body).contains("describe-the-image")).context("model request")?;
        let sent: Value = serde_json::from_slice(&sent.body)?;
        let image = sent["input"].as_array().into_iter().flatten()
            .flat_map(|item| item["content"].as_array().cloned().unwrap_or_default())
            .find(|part| part["type"] == "input_image").context("image part")?;
        assert_eq!(image["image_url"], json!(format!("data:image/png;base64,{PNG}")));

        // Pages that are not images, failed fetches, and redirects are refused
        // before anything is dispatched.
        let send = |input: Value| client.post(format!("{url}/events")).bearer_auth(TOKEN)
            .json(&json!({"events":[{"type":"agent.session.input.message","input":input}]})).send();
        for (route, message) in [
            ("page.html", "input_image is not an image"),
            ("missing.png", "input_image could not be fetched: the server answered 404"),
            ("moved.png", "input_image could not be fetched: the server answered 302"),
        ] {
            assert_eq!(refused(send(image_input(&format!("{}/{route}", images.uri()))).await?).await?,
                (reqwest::StatusCode::BAD_REQUEST, json!(message)), "{route}");
        }
        // Each image is under 20 MiB, but one request may fetch at most 50 MiB
        // of remote images in total.
        let large = json!({"type":"input_image","image_url":format!("{}/large.png", images.uri())});
        let four = json!([{"role":"user","content":[large, large, large, large]}]);
        assert_eq!(refused(send(four).await?).await?, (reqwest::StatusCode::BAD_REQUEST,
            json!("input_image: a request may fetch at most 50 MiB of remote images")));
        let turns = request(&client, reqwest::Method::GET, &format!("{url}/turns"), Value::Null).await?;
        assert_eq!(turns["data"].as_array().map(Vec::len), Some(1));

        server.abort();
        api.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    }).await?
}
