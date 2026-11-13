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

/// Remote images fetched for one request, as the Responses API accepts.
const MAX_REQUEST_IMAGES: usize = 500;
/// Remote image bytes fetched for one request, as the Responses API accepts.
const MAX_REQUEST_IMAGE_BYTES: usize = 50 << 20;

/// What one request may still fetch. Each image is held, base64-encoded,
/// until the request is dispatched, so the request as a whole is bounded.
pub(crate) struct Budget {
    images: usize,
    bytes: usize,
}

impl Budget {
    pub(crate) fn per_request() -> Self {
        Self {
            images: MAX_REQUEST_IMAGES,
            bytes: MAX_REQUEST_IMAGE_BYTES,
        }
    }
}

/// Replace each remote image in parsed input with the image it names,
/// charging `budget`.
pub(crate) async fn inline(
    state: &State,
    items: &mut [Value],
    budget: &mut Budget,
) -> Result<(), ApiError> {
    for item in items.iter_mut().filter(|item| item["type"] == "image") {
        let Some(url) = item["url"].as_str().filter(|url| !url.starts_with("data:")) else {
            continue;
        };
        if budget.images == 0 {
            return Err(invalid(format!(
                "input_image: a request may fetch at most {MAX_REQUEST_IMAGES} remote images"
            )));
        }
        budget.images -= 1;
        item["url"] = json!(fetch(state, url, budget).await?);
    }
    Ok(())
}

async fn fetch(state: &State, url: &str, budget: &mut Budget) -> Result<String, ApiError> {
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
        if bytes.len() + chunk.len() > budget.bytes {
            return Err(invalid(
                "input_image: a request may fetch at most 50 MiB of remote images",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    budget.bytes -= bytes.len();
    Ok(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
}
