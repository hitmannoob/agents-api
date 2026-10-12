use super::*;
use pretty_assertions::assert_eq;

#[test]
fn only_public_addresses_are_reachable_without_approval() {
    let reachable = [
        "93.184.216.34",
        "8.8.8.8",
        "2606:4700:4700::1111",
        "::ffff:8.8.8.8",
    ];
    let blocked = [
        "127.0.0.1",
        "10.0.0.1",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.0.1",
        "0.0.0.0",
        "255.255.255.255",
        "::1",
        "fd00::1",
        "fe80::1",
        "::ffff:127.0.0.1",
        "::ffff:169.254.169.254",
    ];
    let classify = |addresses: &[&str]| {
        addresses
            .iter()
            .map(|address| public(&address.parse().expect("address")))
            .collect::<Vec<_>>()
    };
    assert_eq!(classify(&reachable), vec![true; reachable.len()]);
    assert_eq!(classify(&blocked), vec![false; blocked.len()]);
}

fn config(tools: Value) -> Result<AgentConfig, ApiError> {
    crate::configuration::configure(
        AgentConfig::default(),
        json!({"model":"mock-model","tools":tools}),
    )
}

#[test]
fn inline_authorization_is_bounded_and_preserves_explicit_schemes() {
    for (value, expected) in [
        ("access-token", "Bearer access-token"),
        ("Bearer access-token", "Bearer access-token"),
        ("Basic dXNlcjpwYXNz", "Basic dXNlcjpwYXNz"),
        ("custom token", "custom token"),
    ] {
        assert_eq!(authorization_header(value), expected);
    }
    assert!(validate_authorization(&"x".repeat(8192)).is_ok());
    for value in [
        String::new(),
        "x".repeat(8193),
        "token\r\nX-Injected: yes".into(),
        "token\0".into(),
    ] {
        let error = validate_authorization(&value).expect_err("invalid authorization");
        assert_eq!(error.0, axum::http::StatusCode::BAD_REQUEST);
        if !value.is_empty() {
            assert!(!error.1.contains(&value));
        }
    }
}

#[test]
fn mcp_secrets_are_extracted_without_changing_env_snapshots() {
    let mut patch = json!({"tools":[
        {"type":"mcp","server_label":"remote","transport":{"type":"http","server_url":"https://example.com/mcp","authorization":"access-token"}},
        {"type":"mcp","server_label":"local","transport":{"type":"stdio","command":"server","cwd":"/","env":{"TOKEN":"old-secret"}}},
        {"type":"mcp","server_label":"none","transport":{"type":"http","server_url":"https://example.com/mcp","authorization":null}}
    ]});
    let values = crate::stdio_env::take(&mut patch)
        .unwrap_or_else(|error| panic!("{}", error.1))
        .expect("tools");
    assert_eq!(
        crate::stdio_env::authorization(&values, "remote"),
        Some("access-token")
    );
    assert_eq!(
        values["local"],
        BTreeMap::from([("TOKEN".into(), "old-secret".into())])
    );
    assert!(!values.contains_key("none"));
    for key in values["remote"].keys() {
        assert!(
            !variable_name(key),
            "reserved secret key must not collide with env names"
        );
    }
    assert!(!patch.to_string().contains("access-token"));
    assert!(!patch.to_string().contains("old-secret"));
    assert!(
        patch["tools"][0]["transport"]
            .get("authorization")
            .is_none()
    );
    let old: crate::stdio_env::Values =
        serde_json::from_value(json!({"local":{"TOKEN":"old-secret"}})).expect("old snapshots");
    assert_eq!(old["local"], values["local"]);
    assert_eq!(
        crate::stdio_env::take(&mut json!({"instructions":"keep tools"}))
            .unwrap_or_else(|error| panic!("{}", error.1)),
        None
    );
    assert_eq!(
        crate::stdio_env::take(&mut json!({"tools":null}))
            .unwrap_or_else(|error| panic!("{}", error.1)),
        Some(BTreeMap::new())
    );
}

#[test]
fn authorization_rejects_conflicts_and_stdio_fields() {
    for authorization in [
        json!(""),
        json!(42),
        json!("bad\r\nheader"),
        json!("x".repeat(8193)),
    ] {
        let mut patch = json!({"tools":[{"type":"mcp","server_label":"remote","transport":{"type":"http","server_url":"https://example.com","authorization":authorization}}]});
        assert_eq!(
            crate::stdio_env::take(&mut patch)
                .expect_err("invalid secret")
                .0,
            axum::http::StatusCode::BAD_REQUEST
        );
    }
    let mut conflict = json!({"tools":[{"type":"mcp","server_label":"remote","credential_id":"cred_1","transport":{"type":"http","server_url":"https://example.com","authorization":"token"}}]});
    let direct = conflict["tools"].clone();
    assert!(
        crate::stdio_env::take(&mut conflict)
            .expect_err("credential conflict")
            .1
            .contains("credential_id")
    );
    assert!(
        config(direct)
            .expect_err("direct parse conflict")
            .1
            .contains("credential_id")
    );
    for name in ["Authorization", "authorization", "AUTHORIZATION"] {
        let error = config(json!([{"type":"mcp","server_label":"remote","transport":{"type":"http","server_url":"https://example.com","headers":{name:"token"}}}])).expect_err("header rejected");
        assert!(error.1.contains("transport.authorization"));
    }
    assert!(config(json!([{"type":"mcp","server_label":"local","transport":{"type":"stdio","command":"server","cwd":"/","authorization":null}}])).is_err());
    for authorization in [json!("token"), Value::Null] {
        let transport: McpTransport = serde_json::from_value(
            json!({"type":"http","server_url":"https://example.com","authorization":authorization}),
        )
        .expect("HTTP authorization");
        assert_eq!(
            serde_json::to_value(transport)
                .expect("transport")
                .get("authorization"),
            None
        );
    }
}

#[test]
fn request_metadata_validates_nested_values_and_limits() {
    let valid = BTreeMap::from([(
        "context".into(),
        json!({"tenant":"a","values":[true,1,1.5,"x",{}]}),
    )]);
    validate_metadata(&valid).unwrap_or_else(|error| panic!("{}", error.1));
    for value in [
        Value::Null,
        json!({"nested":null}),
        json!([{"nested":[null]}]),
    ] {
        let error = validate_metadata(&BTreeMap::from([("context".into(), value)]))
            .expect_err("null rejected");
        assert_eq!(error.0, axum::http::StatusCode::BAD_REQUEST);
        assert!(error.1.contains("JSON null") && error.1.contains("TOML"));
    }
    assert!(config(json!([{"type":"mcp","server_label":"remote","transport":{"type":"http","server_url":"https://example.com"},"request_metadata":null}])).is_err());
    let keys = (0..128)
        .map(|i| (format!("key{i}"), json!(i)))
        .collect::<BTreeMap<_, _>>();
    validate_metadata(&keys).unwrap_or_else(|error| panic!("{}", error.1));
    assert!(
        validate_metadata(&BTreeMap::from([("nested".into(), json!([keys]))]))
            .expect_err("nested key count")
            .1
            .contains("128 keys")
    );
    validate_metadata(&BTreeMap::from([("x".into(), json!("x".repeat(16376)))]))
        .unwrap_or_else(|error| panic!("{}", error.1));
    assert!(
        validate_metadata(&BTreeMap::from([("x".into(), json!("x".repeat(16377)))]))
            .expect_err("byte count")
            .1
            .contains("16384 bytes")
    );
    assert!(
        validate_metadata(&BTreeMap::from([("integer".into(), json!(u64::MAX))]))
            .expect_err("TOML integer overflow")
            .1
            .contains("64-bit")
    );
}

#[test]
fn overrides_forward_metadata_and_prefer_inline_authorization() {
    let metadata = json!({"context":{"tenant":"a"},"flags":[true,false]});
    let mut patch = json!({"tools":[
        {"type":"mcp","server_label":"remote","transport":{"type":"http","server_url":"https://example.com","authorization":"Basic abc","headers":{"x-tenant":"a"}},"request_metadata":metadata},
        {"type":"mcp","server_label":"local","transport":{"type":"stdio","command":"server","cwd":"/","env":{"TOKEN":"old-secret"}}}
    ]});
    let secrets = crate::stdio_env::take(&mut patch)
        .unwrap_or_else(|error| panic!("{}", error.1))
        .expect("tools");
    let config = config(patch["tools"].clone()).unwrap_or_else(|error| panic!("{}", error.1));
    let tokens = BTreeMap::from([("remote".into(), "vault-token".into())]);
    let entries = overrides(
        &config,
        &tokens,
        &secrets,
        /*environment_id*/ Some("executor"),
    );
    assert_eq!(
        entries["remote"]["http_headers"],
        json!({"Authorization":"Basic abc","x-tenant":"a"})
    );
    assert_eq!(entries["remote"]["request_meta"], metadata);
    assert_eq!(entries["local"]["env"], json!({"TOKEN":"old-secret"}));
    assert_eq!(entries["local"]["environment_id"], "executor");
    let entries = overrides(
        &config,
        &tokens,
        &BTreeMap::new(),
        /*environment_id*/ None,
    );
    assert_eq!(
        entries["remote"]["http_headers"]["Authorization"],
        "Bearer vault-token"
    );
    assert!(!entries.contains_key("local"));
}
