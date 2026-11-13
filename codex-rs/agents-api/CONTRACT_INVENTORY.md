# Agents API contract inventory

Updated: 2026-11-13 (contract review date 2026-09-22).

This is the pinned G00 parity denominator for this service. The machine-readable
inventory is [CONTRACT_INVENTORY.json](CONTRACT_INVENTORY.json). It records every
HTTP operation exposed by the Agents API resource tree in `openai==3.17.0`, the
trace-export route documented outside that SDK surface, and the Files and Skills
operations required by documented Agents API workflows.

## Baseline and scope

- North star: [Agents](https://developers.openai.com/api/docs/guides/agents).
- Contract guides: the Agents API pages in the
  [official guide index](https://developers.openai.com/api/docs/llms.txt).
- SDK baseline: official Python SDK `openai==3.17.0`.
- Review date: 2026-09-22.
- The pinned SDK sends `OpenAI-Beta: agents=v1` on Agents API requests, but
  not on the Files, Skills, or Webhooks dependencies. This service accepts and
  ignores the header on every route (decision 2026-10-12).
- Windows and cross-OS worker/executor runs, including Windows orphan
  containment, are outside the parity target by project decision (2026-10-13).

The operation denominator is 67:

- 43 Agents API operations, including raw-HTTP trace export.
- 5 Files API operations needed for `file_id` environment inputs and cleanup.
- 11 Skills API operations needed for hosted `skill_reference` inputs and
  version selection. This service has no hosted environments, so it accepts
  the same references on `self_hosted` environments as a local extension
  (see the README).
- 8 Webhooks API operations (endpoints and event types). The session webhook
  guide has callers create an endpoint and select Agents API events.

The SDK resource surface contains 66 of those operations. Trace export is
documented as `GET /v1/agents/sessions/{session_id}/traces` but has no Python
SDK 3.17.0 resource method, so it stays in the inventory with raw-HTTP
acceptance coverage required.

Provider guides for Blaxel, Cloudflare, Daytona, DigitalOcean, E2B, Modal, OCI,
Runloop, and Vercel were reviewed as integrations with the common self-hosted
environment contract. They do not add service endpoints to this denominator.

## Status accounting

Statuses apply to the entire inventoried operation, including its request and
response variants and documented errors:

- `missing`: no target-contract implementation.
- `partial`: a useful subset works, but one or more inventoried variants or
  failure behaviors are absent.
- `implemented`: the code path is complete, but required acceptance evidence is
  not yet available.
- `verified`: the complete operation passed its required behavioral contract
  tests in the recorded environment.

The current count (2026-11-13) is 62 verified, 0 implemented, 0 partial, and
5 missing. The 5 missing are the environment template operations, which are out
of scope since 2026-10-06. Verified: ART-001..004, SUB-001..006, TRN-002,
AGT-001..005, SES-001..005, EVT-001..002, ENV-001..003,
ITM-001, TRN-001, VLT-001..004, CRD-001..005, FIL-001..005, WHK-001..008,
TRC-001, SKL-001..006, and SKV-001..005.
ENV-002..003 moved to verified after the 2026-10-13 scope decision removed
their only remaining evidence requirement. AGT-002 and SES-003 moved to verified
on the 2026-11-13 real-provider evidence below. The 18 behavior rows are
16 verified, 0 partial, and 2 missing (BEH-011 out of scope; BEH-018 open).
A partial operation does not count as completed parity.
[PARITY.md](PARITY.md#current-status) has a per-area summary.

BEH-017 is verified locally (2026-10-13): a synced atomic pre-spawn intent and
inherited lock close the spawn-to-record window. Exact command/start-time checks
preserve foreign processes; a lock held past 10 seconds fails startup safely.
Evidence: `runtime::worker_spawned_before_record_is_reclaimed_after_owner_crash`,
`runtime::pid_reused_record_does_not_signal_foreign_process`, and
`runtime::stale_spawn_intent_does_not_signal_another_worker`; recorded macOS
runtime 16/16 and lib 24/24 passed. Linux evidence follows under 2026-11-13.
Lost function waiters stay unresolved/unavailable by decision: callbacks exist
only in memory, resume replays only live requests, and new workers restart their
request-ID counter.
Legacy records remain supported; external workers are untouched. Executor loss
is tracked separately in BEH-010.

EVT-002 is verified locally (2026-10-13):
`environments::self_hosted_command_output_deltas_arrive_in_order_before_item_done`
passed 3/3 macOS runs with a mock provider, including immediate stdout, spaced
stdout/stderr, and fast exit; IDs/output_index and saved output match. Core
`unified_exec` retains the receiver before the early-exit wait and drains before
End. Recorded core evidence: `unified_exec_emits_initial_output_deltas_in_order`,
`exec_server_output_preserves_bytes_before_watcher`, and
`streaming_output_drains_lagged_receiver` (lib 92/92, `--test all unified_exec`
101/101). Best-effort limits: 10,000 deltas per call; more than 64 chunks before
the watcher starts drops the oldest from deltas on lag. Aggregated output is
unaffected; streams remain live-only.

G08 closes AGT-001 and SES-001's last gaps (2026-10-12):
`programmatic::programmatic_tool_calls_reach_the_application_and_respect_the_model`
verifies nested function results, cancellation, isolation, and model-mode
refusals with a mock provider and real V8 host;
`compaction::automatic_compaction_survives_cold_resume` verifies automatic
compaction and complete public history after cold resume. The pinned contract
has no public compaction settings or items. At this milestone BEH-008 stayed
partial for real-provider web search (closed 2026-11-13); inline authorization
and request metadata are verified by
`mcp::mcp_inline_authorization_and_metadata_survive_session_snapshots`
and the authorization unit tests (mock MCP server).

G08 skills/plugins isolation is verified locally (2026-10-13):
`environments::skills_and_plugins_stay_isolated_between_sessions_across_cold_resume`
checks distinct capability-directory skills, plugin skills, and pinned Skills
API versions in two sessions' model-visible developer catalogs before and after
API/worker restart and executor reconnection. Later defaults do not replace the
pins; neither session receives the other's selections. Inventory statuses and
counts are unchanged.

## Linux and real-provider evidence (2026-11-13)

Platform: Modal Sandbox, Linux x86_64 under gVisor (kernel `4.19.0-gvisor`),
16 CPUs, Rust 1.95, built from HEAD `4bbd24459c`. All Linux test runs used
`cargo nextest --retries 0`. The full `codex-agents-api` crate passed 93/93
with `--run-ignored all`, including the three pinned SDK tests. A separate
Sandbox repeated the recovery/streaming slice three times, 43/43 each:
runtime binary 16/16 (including the three BEH-017 tests above), lib 24/24,
and `environments::self_hosted_command_output_deltas_arrive_in_order_before_item_done`.
This adds Linux evidence for BEH-017 and EVT-002 to their earlier macOS runs.

The `codex-core` `-E 'test(/unified_exec/)'` slice passed 184/191. All EVT-002
core tests above passed, including `early_exit` and `later_output` for both
initial-output tests. The seven failures require Codex's Linux sandbox
(bubblewrap), whose namespaces gVisor cannot host: `unshare -n` and bwrap fail
with "Operation not permitted" and "bwrap: loopback: Failed RTM_NEWADDR".
None was touched by this branch; this is an environment limitation, not a
regression. G12's dated evidence in [GOALS.md](GOALS.md#linux-and-real-provider-evidence-2026-11-13)
lists the seven tests.

`tests/real_provider.py` ran inside the service Sandbox started with
`deploy/modal/serve.py`, with `/opt/agents-api/bin/codex` as the stock
`codex exec-server` executor. All eight original scenarios (`plain`, `stream`,
`function`, `structured`, `web_search`, `cancel`, `traces`, `self_hosted`) passed
against `openai/gpt-5-mini` via OpenRouter for 55,145 tokens. The new
`saved_agent` and `session_update` scenarios (commit `e6cc90c2b2`) passed for
16,228 tokens. AGT-002, SES-003, BEH-002, BEH-008, and BEH-010 move to verified;
BEH-010 now has a second real-provider run, its first on Linux.

One web-search session recorded five completed `web_search_call` items with
search actions, including "site:python.org latest Python release", and an
answer citing python.org. Three actions had an empty query. The service maps
Codex's action faithfully (`src/records.rs`, `web_search_action`), so the blank
comes from Codex or the provider; it was not investigated.

The session-update trace's `gen_ai.response.model` is the service record of the
model configured at turn start (`src/routes.rs`, `turn_models` insert), updated
only by a Codex reroute; it is not provider-reported. Independently, Codex's
rollout `turn_context` recorded turn 1 `model=openai/gpt-5-mini` (default effort)
and turn 2 `model=openai/gpt-5-nano effort=low`. Provider-side confirmation of
the serving model was not checked. BEH-018 stays missing/open: a Modal Sandbox
ran the executor, but whether that counts as provider interoperability is an
open project decision. G12's SDK scenario expansion remains unchecked.

## What is pinned

The JSON inventory contains:

- Method, path, SDK resource/method, source URL, implementation location, test,
  evidence type, status, and remaining gap for every operation.
- Exact discriminated-union members for environments, input events, content,
  tools, items, required actions, live events, and webhooks.
- Session, turn, subagent, environment, item, and function-call status values.
- Cross-cutting semantics for omission/null updates, pagination, idempotency,
  recovery, environments, tools, files, credentials, webhooks, usage, tracing,
  and provider interoperability.
- The compatibility-baseline update procedure.

For SDK-backed operations, `sdk_resource` plus `sdk_method` identifies the exact
pinned request signature and response type; transport-only SDK escape hatches
(`extra_headers`, `extra_query`, `extra_body`, and `timeout`) are not product
parameters. The drift check resolves those methods and their generated response
models directly from 3.17.0. Cross-operation constraints and errors live in the
behavior rows because the public Agents guides do not currently publish a
complete per-operation error matrix. Missing error detail is therefore an
explicit coverage gap, not an inferred contract.

Small SDK-shaped request and response examples live in
[`tests/fixtures/sdk_contract.json`](tests/fixtures/sdk_contract.json). Shared
Python helpers in [`tests/sdk_helpers.py`](tests/sdk_helpers.py) keep SDK version
and fixture loading consistent between acceptance scripts.

## Verification

`tests/sdk_inventory.py` validates that:

- The installed SDK is exactly `openai==3.17.0`.
- Every SDK-backed inventory entry resolves to the expected generated resource
  method, HTTP verb, path, and Agents beta header where applicable.
- No HTTP method in a tracked SDK resource class is omitted from the inventory.
- The representative agent and session response fixtures parse through the
  pinned SDK models.

Run it directly:

```sh
python tests/sdk_inventory.py CONTRACT_INVENTORY.json
```

The ignored Rust integration test runs the same check when
`CODEX_AGENTS_API_SDK_PYTHON` points to the pinned Python environment.

## Maintaining the baseline

Do not edit the denominator implicitly while implementing another goal. A
contract update must change the pinned SDK/docs review metadata, run the SDK
inventory diff, record added or removed behavior with a source, update fixtures
and acceptance tests, and explain whether the change is compatible. Mock,
real-provider, and platform evidence remain separate fields; one must not be
substituted for another.

Amendments under the same `openai==3.17.0` pin:
- 2026-09-29: added WHK-001..008, the SDK's `client.webhooks` endpoint and
  event-type operations. The session webhook guide has callers create an
  endpoint and select Agents API events, so these operations are required
  dependencies that the G00 review missed. The change is additive: no existing
  entry changed meaning. `tests/sdk_inventory.py` checks the new resources, and
  `tests/sdk_webhooks.py` validates their responses.
- 2026-10-06: by project decision, service-managed (`openai_hosted`)
  environments are not supported. TPL-001..005 (environment templates) and
  BEH-011 (hosted configuration) leave the parity target. They stay in the
  inventory with status `missing`, so the SDK diff still covers them, and the
  API rejects `openai_hosted` as not supported. The parity target is now the
  rest of the inventory.
- 2026-10-12: project scope enters the parity target (G13). API keys belong
  to projects and carry permissions; every resource is project-scoped, and
  another project's IDs read as not found; a missing permission answers 403.
  Projects and keys are managed by CLI, and the OpenAI Admin API is not
  inventoried. `api.webhooks.*` is a local permission name, because the
  guides name none. The change is additive: the operator token keeps every
  permission in the default project. The second promotion pass moved
  AGT-005, SES-004, VLT-001..004, CRD-002, CRD-004, WHK-002, WHK-004..006, and
  BEH-003 to verified.
- 2026-10-12: executors may authenticate with a project key holding
  `api.environments.connect`, a local name for the guide's "only permits
  connecting environments" key, which must belong to the session's project.
  By project decision, the OpenAI Admin API (`/organization/projects` and
  project API key management) stays outside the denominator; the CLI manages
  projects and keys.
- 2026-10-12: contract decisions with no provider dependency. The
  `OpenAI-Beta` header is accepted and ignored on every route; the
  prototype's agent routes move to `/v1/prototype/agents`. Webhook endpoints
  accept and test every event type the pinned SDK lists, though only
  `agent.session.*` events are produced (additive). Upload parts (the Uploads
  API), batch processing (the Batch API), and file range requests leave the
  parity target (scope reduction). List cursors are resource IDs and do not
  expire. The third promotion pass moved AGT-004, FIL-001..005, WHK-001,
  WHK-003, WHK-008, BEH-001, and BEH-004 to verified.
- 2026-10-12: decisions where the docs define nothing or Codex reports
  nothing. The OTLP trace format, batch and idempotency semantics, and the
  10,000-byte function result limit are the contract. A failed webhook test
  is an error, and delivery order is not guaranteed. Subagent task text,
  close, and `closed` status are Codex engine limits. OAuth token refresh and
  external key management are out of scope (scope reduction). Webhook events
  gained environment data and the actual required action, and trace spans
  the response model (additive). The fourth promotion pass moved WHK-007,
  TRC-001, SUB-001, CRD-001, CRD-003, CRD-005, BEH-005, BEH-009, BEH-013,
  BEH-014, and BEH-016 to verified.
- 2026-10-12: public routes drop the prototype's size limits. Instructions
  and message text take up to 1 MiB, agents up to 128 tools, and agent,
  session, and input requests 8 MiB bodies; the contract documents no
  limits, so these are local bounds sized to Codex's input limit (additive).
  AGT-003 moved to verified.
- 2026-10-12: remote https input images are fetched by the service under
  the egress rules and passed to Codex inline; environments report `failed`
  after a connection timeout, keep "connected once" across restarts, and
  never report `expired`; session deletion was confirmed against the
  lifecycle guide; `create_subagent_call` carries the model Codex reports.
  EVT-001, ENV-001, and SES-005 moved to verified (additive).
- 2026-10-12: input waiting for a self-hosted executor queues a turn whose
  service-chosen ID the started Codex turn keeps; messages between agents are
  `agent_message` items from Codex's experimental raw events, including
  encrypted task text. TRN-001 and ITM-001 moved to verified (additive).
- 2026-10-12: Codex's app-server gains `thread/resume.experimentalRawEvents`,
  so agent messages keep being recorded after a session resumes on a new
  worker connection (additive).
- 2026-10-12: session configuration audited against the pinned SDK; input
  accepts 1-32 user messages, the only missing create variant. SES-002 moved
  to verified; SES-001 remains partial only for capability execution
  (additive).
- 2026-10-12: deferred functions and tool search execute (G06). Codex's
  `model/list` reports `supportsSearchTool`; deferred functions are searched
  in a `deferred` namespace on capable models and offered up front otherwise.
  BEH-007 moved to verified (additive).
- 2026-10-12: G08 programmatic calling executes through the fail-closed V8
  host with ordinary nested function calls, cancellation, and model-mode
  refusals (`model/list.toolMode`). Automatic compaction and cold resume
  preserve public history; compaction has no public controls/items. Inline
  MCP authorization is encrypted and write-only; HTTP transport headers are
  non-secret configuration and round trip. An Authorization header is refused;
  use `transport.authorization`. `request_metadata` reaches tools/call `_meta` through
  `codex-config`/`codex-core` `request_meta`. AGT-001 and SES-001 moved to
  verified; BEH-008 stays partial only for real-provider web search. Evidence
  is local, mock-provider/MCP with the real V8 host (tests named above).
- 2026-10-13: by project decision, Windows and cross-OS worker/executor runs,
  including Windows orphan containment, leave the parity target (scope
  reduction). ENV-002..003 move from implemented to verified because no other
  gaps remain; their recorded evidence is unchanged. BEH-017 loses the Windows
  requirement but stays partial for the spawn-to-record window and its other
  recovery limitations. The operation denominator remains 67: 59 verified,
  0 implemented, 3 partial, and 5 missing. The 18 behavior rows remain
  12 verified, 4 partial, and 2 missing.
- 2026-10-13: BEH-017 and EVT-002 move from partial to verified (additive).
  Managed-worker spawn-to-record recovery and initial/fast-exit command deltas
  have local evidence named above. Lost function waiters remain unresolved/
  unavailable by decision; deltas retain the 10,000-per-call and 64-chunk
  pre-watcher limits. Counts at this milestone: 67 operations, 60 verified,
  0 implemented, 2 partial, 5 missing (out of scope); 18 behaviors, 13 verified,
  3 partial, 2 missing (BEH-011 out of scope; BEH-018 open).
- 2026-11-13: Linux and real-provider evidence recorded above (status only;
  denominator unchanged). AGT-002, SES-003, BEH-002, BEH-008, and BEH-010 move
  to verified. Counts: 67 operations, 62 verified, 0 implemented, 0 partial,
  5 missing (out of scope); 18 behaviors, 16 verified, 0 partial, 2 missing
  (BEH-011 out of scope; BEH-018 open).
