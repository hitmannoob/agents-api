use super::*;
use pretty_assertions::assert_eq;

fn refused(environment: Value) -> String {
    match parse(&environment, WORKSPACE) {
        Ok(_) => panic!("{environment} was accepted"),
        Err(error) => error.1,
    }
}

fn accepted(environment: Value) -> Hosted {
    match parse(&environment, WORKSPACE) {
        Ok(hosted) => hosted,
        Err(error) => panic!("{environment} was refused: {}", error.1),
    }
}

#[test]
fn hosted_configuration_defaults_and_keeps_supported_fields() {
    let hosted = accepted(json!({"type":"openai_hosted"}));
    assert_eq!(
        (
            hosted.container_size.as_str(),
            hosted.packages,
            hosted.env.len(),
            hosted.setup_commands.len()
        ),
        ("medium", Packages::default(), 0, 0)
    );
    let hosted = accepted(json!({"type":"openai_hosted","container_size":"large",
        "packages":{"python":["requests==2.32.3","pkg[extra]>=1"],"npm":["@scope/tool@1.2.0"],"system":["ripgrep"]},
        "env":{"MODE":"test","_TOKEN":"value"},"setup_commands":[{"command":"make setup","cwd":"/workspace/app"},{"command":"true"}],
        "network":{"access":"restricted","allowed_domains":["pypi.org","files.pythonhosted.org"]},
        "files":[{"type":"inline","path":"/workspace/app/a.txt","data":"aGk="},{"type":"file_id","path":"/workspace/b.csv","file_id":"file-1"}],
        "skills":null,"desktop":{"enabled":false}}));
    assert_eq!(
        hosted
            .files
            .iter()
            .map(|file| (
                file.path.as_str(),
                file.file_id.as_deref(),
                file.contents.as_slice()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("/workspace/app/a.txt", None, b"hi".as_slice()),
            ("/workspace/b.csv", Some("file-1"), b"".as_slice())
        ]
    );
    assert_eq!(
        (
            hosted.network.access.as_str(),
            hosted.network.allowed_domains.len()
        ),
        ("restricted", 2)
    );
    assert_eq!(
        (
            hosted.container_size.as_str(),
            hosted.packages.python.len(),
            hosted.env["MODE"].as_str(),
            hosted.setup_commands[0].cwd.as_deref(),
            hosted.setup_commands[1].cwd.as_deref()
        ),
        ("large", 2, "test", Some("/workspace/app"), None)
    );
}

#[test]
fn unsupported_or_unsafe_hosted_configuration_is_refused() {
    for (environment, message) in [
        (
            json!({"type":"openai_hosted","environment_template_id":"tpl_1"}),
            "field environment_template_id is not supported",
        ),
        (
            json!({"type":"openai_hosted","environment_id":"env_1"}),
            "field environment_id is not supported",
        ),
        (
            json!({"type":"openai_hosted","desktop":{"enabled":true}}),
            "field desktop is not supported",
        ),
        (
            json!({"type":"openai_hosted","container_size":"huge"}),
            "container_size must be",
        ),
        // Modal enforces allowlists only, so a denylist is refused.
        (
            json!({"type":"openai_hosted","network":{"access":"restricted","blocked_domains":["a.com"]}}),
            "blocked_domains is not supported",
        ),
        (
            json!({"type":"openai_hosted","network":{"access":"enabled","allowed_domains":["pypi.org"]}}),
            "requires access restricted",
        ),
        (
            json!({"type":"openai_hosted","network":{"access":"restricted","allowed_domains":["*.pypi.org"]}}),
            "without wildcards",
        ),
        (
            json!({"type":"openai_hosted","network":{"access":"restricted","allowed_domains":["https://pypi.org"]}}),
            "without wildcards",
        ),
        (
            json!({"type":"openai_hosted","network":{"access":"open"}}),
            "network.access must be",
        ),
        // Files land inside the workspace, once each.
        (
            json!({"type":"openai_hosted","files":[{"type":"inline","path":"/etc/passwd","data":"YQ=="}]}),
            "must be an absolute path inside /workspace",
        ),
        (
            json!({"type":"openai_hosted","files":[{"type":"inline","path":"/workspace/../etc/x","data":"YQ=="}]}),
            "must be an absolute path inside /workspace",
        ),
        (
            json!({"type":"openai_hosted","files":[{"type":"inline","path":"/workspace","data":"YQ=="}]}),
            "must be an absolute path inside /workspace",
        ),
        (
            json!({"type":"openai_hosted","files":[{"type":"inline","path":"/workspace/a","data":"YQ=="},
                {"type":"file_id","path":"/workspace/a","file_id":"file-x"}]}),
            "listed twice",
        ),
        (
            json!({"type":"openai_hosted","files":[{"type":"inline","path":"/workspace/a","data":"not base64!"}]}),
            "standard base64",
        ),
        (
            json!({"type":"openai_hosted","files":[{"type":"url","path":"/workspace/a"}]}),
            "files must be a list",
        ),
        // A package is one installer argument: no options, quotes, or shell syntax.
        (
            json!({"type":"openai_hosted","packages":{"system":["--allow-unauthenticated"]}}),
            "packages must name",
        ),
        (
            json!({"type":"openai_hosted","packages":{"npm":["x; curl evil"]}}),
            "packages must name",
        ),
        (
            json!({"type":"openai_hosted","packages":{"python":["$(id)"]}}),
            "packages must name",
        ),
        (
            json!({"type":"openai_hosted","packages":{"ruby":["x"]}}),
            "packages takes",
        ),
        // The executor's credentials cannot be overridden.
        (
            json!({"type":"openai_hosted","env":{"CODEX_API_KEY":"x"}}),
            "env takes",
        ),
        (
            json!({"type":"openai_hosted","env":{"AGENTS_API_REGISTRY_URL":"x"}}),
            "env takes",
        ),
        (
            json!({"type":"openai_hosted","env":{"1BAD":"x"}}),
            "env takes",
        ),
        (
            json!({"type":"openai_hosted","env":{"A":1}}),
            "env must map",
        ),
        (
            json!({"type":"openai_hosted","setup_commands":[{"command":"  "}]}),
            "setup_commands takes",
        ),
        (
            json!({"type":"openai_hosted","setup_commands":[{"command":"x","cwd":"relative"}]}),
            "setup_commands takes",
        ),
        (
            json!({"type":"openai_hosted","setup_commands":[{"command":"x","shell":"zsh"}]}),
            "setup_commands must be",
        ),
    ] {
        let error = refused(environment.clone());
        assert!(error.contains(message), "{environment}: {error}");
    }
}

fn provision_request(environment_id: &str) -> ProvisionRequest {
    ProvisionRequest {
        environment_id: environment_id.to_owned(),
        registry_url: "https://agents.example/registry".to_owned(),
        token: "envtok_test".to_owned(),
        container_size: "medium".to_owned(),
        packages: Packages::default(),
        env: BTreeMap::new(),
        files: Vec::new(),
        setup_commands: Vec::new(),
        network: Network {
            access: "enabled".to_owned(),
            allowed_domains: Vec::new(),
        },
    }
}

/// Answers each provision with a sandbox named for its request ID, and each
/// destroy with an error, after which it exits.
#[cfg(unix)]
const FAKE_PROVIDER: &str = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  case "$line" in
    *'"op":"destroy"'*) printf '{"id":%s,"ok":false,"error":"no such sandbox"}\n' "$id"; exit 0;;
  esac
  printf '{"id":%s,"ok":true,"sandbox_id":"sb-%s"}\n' "$id" "$id"
done"#;

#[cfg(unix)]
#[tokio::test]
async fn sidecar_provider_routes_answers_and_restarts_after_exit() -> anyhow::Result<()> {
    let provider = SidecarProvider::new(
        "sh".to_owned(),
        vec!["-c".to_owned(), FAKE_PROVIDER.to_owned()],
    );
    // Concurrent calls each get their own answer.
    let (first, second) = tokio::join!(
        provider.provision(provision_request("env_a")),
        provider.provision(provision_request("env_b"))
    );
    let mut sandboxes = vec![first?, second?];
    sandboxes.sort();
    assert_eq!(sandboxes, vec!["sb-1".to_owned(), "sb-2".to_owned()]);
    // An error answer fails the call with the provider's message.
    let destroyed = provider.destroy("sb-1".to_owned()).await;
    assert_eq!(
        destroyed.map_err(|error| error.to_string()),
        Err("no such sandbox".to_owned())
    );
    // The provider exited after answering; a later call starts it again. A
    // call that races the exit fails, as a crash would.
    let mut restarted = None;
    for _ in 0..50 {
        if let Ok(sandbox) = provider.provision(provision_request("env_c")).await {
            restarted = Some(sandbox);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(restarted.is_some_and(|sandbox| sandbox.starts_with("sb-")));
    Ok(())
}
