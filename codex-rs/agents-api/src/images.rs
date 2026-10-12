//! Remote input images. Codex accepts only inline images, so the service
//! fetches an `input_image` URL itself, under the egress rules in
//! [`crate::egress`], and passes the image on as a data URL.
use crate::ApiError;
use crate::State;
use crate::contract::invalid;
use crate::egress::Refusal;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures::StreamExt;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;

/// Largest image fetched, as the Responses API accepts.
pub(crate) const MAX_IMAGE_BYTES: usize = 20 << 20;
const FETCH_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 15);

/// Replace each remote image in parsed input with the image it names.
pub(crate) async fn inline(state: &State, items: &mut [Value]) -> Result<(), ApiError> {
    for item in items.iter_mut().filter(|item| item["type"] == "image") {
        let Some(url) = item["url"].as_str().filter(|url| !url.starts_with("data:")) else {
            continue;
        };
        item["url"] = json!(fetch(state, url).await?);
    }
    Ok(())
}

async fn fetch(state: &State, url: &str) -> Result<String, ApiError> {
    let refused = |reason: String| invalid(format!("input_image {reason}"));
    let (host, addresses) = crate::egress::destination(url, &state.image_hosts)
        .await
        .map_err(|refusal| {
            refused(match refusal {
                Refusal::Url => {
                    "image_url must be a data:image URL or an absolute https URL".into()
                }
                Refusal::Unresolved(host) => format!("host {host} did not resolve"),
                Refusal::NonPublic => {
                    "image_url resolves to a non-public address; the operator must allow its host"
                        .into()
                }
            })
        })?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(FETCH_TIMEOUT)
        .resolve_to_addrs(&host, &addresses)
        .build()
        .map_err(anyhow::Error::from)?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| refused("could not be fetched".into()))?;
    if !response.status().is_success() {
        return Err(refused(format!(
            "could not be fetched: the server answered {}",
            response.status().as_u16()
        )));
    }
    let mime = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|mime| mime.starts_with("image/"))
        .ok_or_else(|| refused("is not an image".into()))?
        .to_owned();
    let mut bytes = Vec::new();
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|_| refused("could not be fetched".into()))?;
        if bytes.len() + chunk.len() > MAX_IMAGE_BYTES {
            return Err(refused("is larger than 20 MiB".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
}
