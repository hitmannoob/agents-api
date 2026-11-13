# Agents API on Modal

Updated: 2026-11-13.

Tools for hosting the Agents API in a [Modal](https://modal.com) Sandbox and
evaluating it with coding tasks that need Codex's harness. They are not part of
the service and are not run by its tests.

## Requirements

- The `modal` Python package, and a Modal token (`modal token new`).
- A Modal secret holding the worker's provider key, created by you so the
  scripts never read it:
  `modal secret create agents-api-openrouter OPENROUTER_API_KEY=…`
- `openai==3.17.0` for the eval runner.

## Hosting

`serve.py` builds `codex-agents-api`, `codex-app-server`, `codex-code-mode-host`,
and the `codex` CLI from the repository's committed `HEAD` into a cached image
layer (about 20–40 minutes on 16 CPUs the first time; later starts take seconds).
The code-mode host sits alongside the app-server and uses Codex's V8 artifacts
verified against the repository's trusted checksums. The Sandbox has 4 CPUs
and 8 GB:

    python serve.py start STATE_DIR     # prints the https endpoint
    python serve.py logs STATE_DIR
    python serve.py stop STATE_DIR

- **Credentials:** a fresh API token, vault passphrase, and environment key
  are written to `STATE_DIR`; keep it private.
- **Networking:** the service binds loopback only, as it requires, and a TCP
  forwarder exposes it on the tunnel.
- **Lifetime:** the Sandbox stops itself after `--hours` (default 3) and
  bills while it runs.

Modal's tunnel sends no `x-forwarded-proto`. `--public-url`
(`CODEX_AGENTS_API_PUBLIC_URL`, added 2026-11-13 in `c222662345`) sets the public
http(s) origin for executor-facing URLs: the session's `remote_url` and the
executor rendezvous URL. `serve.py` passes the https tunnel URL, so executors
can use the returned URLs directly. The harness rendezvous stays request-derived
because the worker reaches the registry over loopback. Outside executors still
need `--trusted-registry-host <tunnel host>`; otherwise the stock CLI sends the
key only to OpenAI or loopback hosts. The option takes an origin with an optional
port, without userinfo, a path other than `/`, query, or fragment.

`serve.py start STATE_DIR --auto-compact-tokens N` (added in `3b5b242c31`,
2026-11-13) sets the worker's `model_auto_compact_token_limit`. The real-provider
compaction scenario opts in with `AGENTS_API_COMPACT_TOKENS` set to that limit.

Deployment caveat (2026-11-13): Modal Sandboxes run under gVisor, which forbids
the namespaces Codex's Linux sandbox (bubblewrap) needs. `serve.py` sets
`sandbox_mode = "read-only"`, which cannot be enforced there: anything the
worker runs under Codex's sandbox fails. `unshare -n` and bwrap namespace
creation fail with "Operation not permitted"; Codex reports
"bwrap: loopback: Failed RTM_NEWADDR". Self-hosted sessions are unaffected
because their commands run on the executor, which runs unsandboxed.

## Linux and real-provider evidence (2026-11-13)

Linux test platform: Modal Sandbox, Linux x86_64 under gVisor (kernel
`4.19.0-gvisor`), 16 CPUs, Rust 1.95, built from HEAD `4bbd24459c`. All test
runs used `cargo nextest --retries 0`. The full `codex-agents-api` crate passed
93/93 with `--run-ignored all`, including three pinned SDK tests. A separate
Sandbox repeated recovery/streaming three times, 43/43 each (runtime binary
16/16, lib 24/24, and the self-hosted command-output test). The core
`-E 'test(/unified_exec/)'` slice passed 184/191; all cited EVT-002 tests passed.
The seven failures require the bubblewrap namespaces above; none was touched
by this branch. This is an environment limitation, not a regression; the exact
tests are recorded in [GOALS.md](../../GOALS.md#linux-and-real-provider-evidence-2026-11-13).

Real-provider run: the service was started with `deploy/modal/serve.py`.
`tests/real_provider.py` ran inside the service Sandbox, with
`/opt/agents-api/bin/codex` as the stock `codex exec-server` executor, so
`self_hosted` ran on Linux. Against `openai/gpt-5-mini` via OpenRouter:

- All eight original scenarios passed (`plain`, `stream`, `function`,
  `structured`, `web_search`, `cancel`, `traces`, `self_hosted`): 55,145 tokens.
- `saved_agent` and `session_update` (commit `e6cc90c2b2`) passed: 16,228 tokens.

Initial web-search observation (2026-11-13): one session recorded five completed
`web_search_call` search actions, including "site:python.org latest Python
release", and an answer citing python.org; three actions had empty queries.
Follow-up (2026-11-13, `7fc4cd1605`, Modal via `serve.py`, `openai/gpt-5-mini`
via OpenRouter): all seven sessions completed with answers; 12 of 23 items (52%)
were `search` actions with query `""`, matching Codex rollout items one-to-one.
Codex omits absent optional fields, so the blanks came from upstream; no
`open_page` or `find_in_page` arrived, making OpenRouter action details lossy.
Page-open flattening is likely but unconfirmed; a direct-OpenAI comparison was
not done. [GOALS.md](../../GOALS.md#linux-and-real-provider-evidence-2026-11-13)
records the fuller evidence.

The session-update trace's `gen_ai.response.model` records the service's
configured model at turn start (`src/routes.rs`, `turn_models` insert), updated
only by a Codex reroute; it is not provider-reported. Independent rollout
`turn_context` recorded turn 1 `model=openai/gpt-5-mini` (default effort) and
turn 2 `model=openai/gpt-5-nano effort=low`. Provider-side confirmation of the
serving model was not checked. This earlier same-Sandbox loopback run did not
supply BEH-018's provider-interoperability evidence; the separate-Sandbox run
below does.

Public-URL fix verification (2026-11-13, Linux Modal): full crate 99/99 with
`--run-ignored all`, crate clippy clean. Then the full `tests/real_provider.py`
suite passed all ten scenarios against the patched service for 81,365 tokens,
after `eae7869510` made `self_hosted` pass `--trusted-registry-host` for a
non-loopback advertised host.

## MCP, subagents, and compaction (2026-11-13)

Commit `3b5b242c31` added the `mcp`, `subagents`, and `compaction` scenarios
in `tests/real_provider.py`, and `serve.py --auto-compact-tokens`. The suite ran
inside the service Sandbox on Modal, started by `serve.py` at image commit
`7fc4cd1605`, against `openai/gpt-5-mini` via OpenRouter:

- `mcp` PASS: a stdio MCP server ran on the self-hosted executor; the real
  model's `mcp_call` returned a value it could not guess.
- `subagents` PASS: with `multi_agent` enabled, the run checked the subagent
  resource, `create_subagent_call`, subagent turns/items, `agent_message` items,
  and correct delegated answer `391`. MCP and subagents used 39,184 tokens together.
- `compaction` PASS on a separate service with
  `model_auto_compact_token_limit=12000` (70,022 tokens): a turn's `input_tokens`
  dropped after compaction, `/items` kept every user message in order, and the
  model recalled the turn-1 code word afterwards.

The first compaction attempt used filler in USER messages and a 20000 limit.
The rollout recorded `compacted` in turn 4, but input never shrank. Codex keeps
recent user messages verbatim through compaction up to
`COMPACT_USER_MESSAGE_MAX_TOKENS = 20_000` (`core/src/compact.rs:63`), and a
compacting turn's usage sums the compaction and follow-up requests. The scenario
therefore grows context via assistant output. This adds real-provider evidence
for MCP (BEH-008), subagents (SUB rows/BEH-009), and compaction (G08);
statuses and counts are unchanged.

## Executor interoperability

`executor_interop.py` runs the stock executor in its own Modal Sandbox and
connects it to the service through the public https tunnel. Run from this
directory against a service started by `serve.py`; `modal` and `openai==3.17.0`
must be installed in the same Python environment:

    python executor_interop.py STATE_DIR [--model M] [--json out.json]

Evidence (2026-11-13, `7fc4cd1605`, patched service,
`openai/gpt-5-mini` via OpenRouter): all seven checks PASS.

- `create`: `remote_url` is https on the tunnel host.
- `connect`: separate executor Sandbox connects through the public tunnel (9.5s).
- `task`: environment-files copy, one `command_execution` item, `result.txt = 100`.
- `disconnect`: killed executor is reported disconnected.
- `waiting_input`: `requires_action` with `environment_connection`, queued turn.
- `reconnect`: the same queued turn completes; `again.txt = again`.
- `delete`: session deleted; executor Sandbox still running.

Project decision (2026-11-13): this separate-Sandbox public-tunnel run satisfies
BEH-018. Modal is one of the provider guides reviewed. The earlier same-Sandbox
loopback run does not supply that evidence.

## Harness evals

`harness_evals.py` runs eight coding tasks:
- write and run a script;
- fix a failing test without touching it;
- compute a CSV statistic;
- rename a function across files;
- search a tree;
- branch and commit;
- filter JSON;
- fix a crash.

Each task gets its own workspace, self-hosted session, and executor. A
checker decides the result, not the model's own report:

    python harness_evals.py STATE_DIR --json modal.json             # Modal
    python harness_evals.py ~/local-deployment --local --json local.json

Local mode needs a running service, its `token` and `environment-key` in the
given directory, and a `codex` binary (default: this checkout's debug build).
Results report pass/fail, turn status, commands run, tokens, and wall time per
task, with a token budget (`--budget`, default 400,000).

First comparison (2026-10-07, `openai/gpt-5-mini`, one run each):
- **Modal:** 8/8, 116 s, 180,697 tokens.
- **Local macOS:** 8/8, 162 s, 237,472 tokens.

Most of the gap came from the model running extra commands locally. A single
run cannot separate network latency, shell differences (`bash` on Linux,
`zsh` on macOS), and model variance.
