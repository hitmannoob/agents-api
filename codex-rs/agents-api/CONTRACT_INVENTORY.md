# Agents API contract inventory

Updated: 2026-10-12 (contract review date 2026-09-22).

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

The current count (2026-10-12) is 57 verified, 2 implemented, 3 partial, and
5 missing. The 5 missing are the environment template operations, which are out
of scope since 2026-10-06. Verified: ART-001..004, SUB-001..006, TRN-002,
AGT-001, AGT-003..005, SES-001..002, SES-004..005, EVT-001, ENV-001, ITM-001, TRN-001, VLT-001..004, CRD-001..005, FIL-001..005,
WHK-001..008, TRC-001, SKL-001..006, and SKV-001..005. Implemented, awaiting cross-OS
evidence: ENV-002..003. A partial operation does not count as completed parity. [PARITY.md](PARITY.md#current-status) has a
per-area summary.

G08 closes AGT-001 and SES-001's last gaps (2026-10-12):
`programmatic::programmatic_tool_calls_reach_the_application_and_respect_the_model`
verifies nested function results, cancellation, isolation, and model-mode
refusals with a mock provider and real V8 host;
`compaction::automatic_compaction_survives_cold_resume` verifies automatic
compaction and complete public history after cold resume. The pinned contract
has no public compaction settings or items. BEH-008 stays partial only for a
real-provider web search run; inline authorization and request metadata are
verified by `mcp::mcp_inline_authorization_and_metadata_survive_session_snapshots`
and the authorization unit tests (mock MCP server).

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
