use super::*;
use pretty_assertions::assert_eq;

#[test]
fn public_url_overrides_executor_urls_but_not_the_harness_url() -> anyhow::Result<()> {
    for (public_url, http_origin, websocket_origin) in [
        (
            "https://public.example",
            "https://public.example",
            "wss://public.example",
        ),
        (
            "https://public.example:8443/",
            "https://public.example:8443",
            "wss://public.example:8443",
        ),
        (
            "http://public.example:8080/",
            "http://public.example:8080",
            "ws://public.example:8080",
        ),
        (
            "https://[2001:db8::1]:8443/",
            "https://[2001:db8::1]:8443",
            "wss://[2001:db8::1]:8443",
        ),
    ] {
        let public_url = parse_public_url(public_url)?;
        for (forwarded, harness_scheme) in
            [(None, "ws"), (Some("http"), "ws"), (Some("https"), "wss")]
        {
            let mut headers = HeaderMap::new();
            headers.insert(axum::http::header::HOST, "127.0.0.1:4501".parse()?);
            if let Some(forwarded) = forwarded {
                headers.insert("x-forwarded-proto", forwarded.parse()?);
            }
            assert_eq!(
                url_for(Some(&public_url), &headers, ("https", "http"), "/registry"),
                format!("{http_origin}/registry")
            );
            let executor_path = "/registry/rendezvous/env/executor?registration=reg&token=exe";
            assert_eq!(
                websocket_url(Some(&public_url), &headers, executor_path),
                format!("{websocket_origin}{executor_path}")
            );
            // As in connect, omit the public origin for the worker's harness
            // even though executor-facing URLs above use it.
            let harness_path = "/registry/rendezvous/env/harness?authorization=hka";
            assert_eq!(
                websocket_url(None, &headers, harness_path),
                format!("{harness_scheme}://127.0.0.1:4501{harness_path}")
            );
        }
    }
    Ok(())
}

#[test]
fn unconfigured_urls_preserve_host_and_forwarded_proto_behavior() -> anyhow::Result<()> {
    let mut headers = HeaderMap::new();
    assert_eq!(
        url_for(None, &headers, ("https", "http"), "/registry"),
        "http://127.0.0.1/registry"
    );
    headers.insert(axum::http::header::HOST, "internal.example:4501".parse()?);
    for (forwarded, http_scheme, websocket_scheme) in [
        (None, "http", "ws"),
        (Some("http"), "http", "ws"),
        (Some("https"), "https", "wss"),
        (Some("HTTPS"), "http", "ws"),
        (Some("https,http"), "http", "ws"),
    ] {
        headers.remove("x-forwarded-proto");
        if let Some(forwarded) = forwarded {
            headers.insert("x-forwarded-proto", forwarded.parse()?);
        }
        assert_eq!(
            url_for(None, &headers, ("https", "http"), "/registry"),
            format!("{http_scheme}://internal.example:4501/registry")
        );
        assert_eq!(
            websocket_url(None, &headers, "/registry/rendezvous/env/executor"),
            format!("{websocket_scheme}://internal.example:4501/registry/rendezvous/env/executor")
        );
    }
    Ok(())
}

#[test]
fn public_url_rejects_non_origins() {
    for value in [
        "",
        "public.example",
        "/registry",
        "//public.example",
        "https:",
        "https:public.example",
        "https:///public.example",
        "ftp://public.example",
        "https://public.example:invalid",
        "https://public.example:65536",
        "https://public.example/registry",
        "https://public.example/path/..",
        "https://public.example//",
        "https://public.example?query=value",
        "https://public.example/?",
        "https://public.example#fragment",
        "https://public.example/#",
        "https://user@public.example",
        "https://user:password@public.example",
        "https://:password@public.example",
        "https://@public.example",
        "https://public.example\\",
    ] {
        let error = parse_public_url(value).expect_err(value);
        assert!(
            error.to_string().starts_with("public URL must be "),
            "{value}: {error}"
        );
    }
}
