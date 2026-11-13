# Agents API functional parity target

Decision: reproduce the documented Agents API behavior on our infrastructure,
using this Codex checkout as the execution engine. The existing HTTP facade is
an implementation prototype, not the target public contract.

Baseline reviewed: 2026-09-22, official Python SDK `openai==3.17.0` and the
official Agents API guides. The [contract inventory](CONTRACT_INVENTORY.md)
establishes a 67-operation denominator: 43 Agents API operations and 24 Files,
Skills, and Webhooks operations required by documented workflows. The text/function
session slice has an executable SDK contract check (`tests/sdk_lifecycle.py`)
with strict response validation; `tests/sdk_inventory.py` detects drift in the
pinned SDK resource surface.

Detailed implementation steps, dependencies, and acceptance gates are maintained
in [GOALS.md](GOALS.md).

## Definition of parity

- Match public operations, request/response schemas, event types, status
  transitions, pagination, configuration semantics, and documented failure behavior.
- Run supported official SDK examples against our service by changing the base
  URL and credentials. SDK compatibility is an acceptance target, not a current claim.
- Own harness startup, worker supervision, session routing, and persistent state.
- Supply our own implementation for managed compute and external service
  integrations. Preserve the public contract where compatibility is intended;
  explicitly track any provider-specific feature that is not implemented.
- Mark each operation implemented only after a behavioral contract test passes.
  Mock-provider tests and real-provider acceptance are separate evidence.
- Do not infer private OpenAI implementation details or promise stronger recovery
  guarantees than the documented contract.
- Windows and cross-OS worker/executor runs, including Windows orphan
  containment, are outside the parity target by project decision (2026-10-13).

## Current status

Updated: 2026-11-13. Counts come from [CONTRACT_INVENTORY.json](CONTRACT_INVENTORY.json);
per-operation detail lives in its `remaining` fields.

**Scorecard:** 67 operations: 62 `verified`, 0 `implemented`, 0 `partial`,
5 `missing`. The 5 missing (environment templates) are outside the parity
target by decision (2026-10-06), so **no in-scope operation is missing**. Of 18
behavior rows, 16 are verified (BEH-001..010, BEH-012..017), 0 are partial,
and 2 are missing (BEH-011 is out of scope; BEH-018, provider interoperability,
is open).

Two promotion passes on 2026-10-12 moved rows whose only gaps were out of
scope, intended behavior, or project scope, which G13 closed: API keys belong
to projects, every resource is project-scoped, and permissions are enforced.
A third pass the same day closed the contract gaps that needed no provider:
webhook event types, the beta header (accepted and ignored everywhere),
pagination evidence, cursor expiry, and the Files scope decision. A fourth
pass recorded decisions where the docs define nothing or Codex reports
nothing (trace format, batching and idempotency, subagent engine limits,
OAuth refresh), and added environment data to webhook events and the model
to trace spans.
The 2026-10-13 scope decision moves ENV-002..003 to verified on their recorded
evidence. The same day's recovery/streaming slices verify BEH-017 and EVT-002:
the spawn-to-record window is closed, and initial/fast-exit command output
streams before `item.done` (test names in the inventory). Lost function waiters
remain unresolved/unavailable by decision. That milestone recorded macOS
recovery evidence; Linux evidence follows below.

Evidence (2026-11-13, Linux and real providers): Modal Sandbox, Linux x86_64
under gVisor (kernel `4.19.0-gvisor`), 16 CPUs, Rust 1.95, built from HEAD
`4bbd24459c`; `cargo nextest --retries 0`. The full crate passed 93/93 with
`--run-ignored all`, including three pinned SDK tests. On a separate Sandbox,
the recovery/streaming slice passed 43/43 three times (runtime 16/16, lib 24/24,
and the self-hosted command-output test), adding Linux evidence for BEH-017
and EVT-002. The core `unified_exec` slice passed 184/191; all cited EVT-002
core tests passed. Seven untouched tests need bubblewrap namespaces that
gVisor cannot host (`unshare -n`/bwrap: "Operation not permitted";
"bwrap: loopback: Failed RTM_NEWADDR"): an environment limitation, not a
regression. [GOALS.md](GOALS.md#linux-and-real-provider-evidence-2026-11-13)
records the exact tests.

`tests/real_provider.py` ran inside the service Sandbox started with
`deploy/modal/serve.py`, using `/opt/agents-api/bin/codex` as the stock
`codex exec-server` executor, so `self_hosted` ran on Linux. All eight original
scenarios passed on `openai/gpt-5-mini` via OpenRouter for 55,145 tokens;
`saved_agent` and `session_update` (commit `e6cc90c2b2`) passed for 16,228 tokens.
AGT-002, SES-003, BEH-002, BEH-008, and BEH-010 move to verified. BEH-010 has a
second real-provider run, its first on Linux. Only BEH-018 remains non-verified
within scope: whether the Modal executor run counts as provider interoperability
is an open project decision. SDK scenarios for every resource remain open.

One web-search session recorded five completed search actions and an answer
citing python.org; three actions had empty queries. `src/records.rs`
`web_search_action` maps Codex's action faithfully, so the blank comes from
Codex or the provider; it was not investigated. For `session_update`, trace
`gen_ai.response.model` is the service record of the model configured at turn
start (`src/routes.rs`, `turn_models` insert), updated only by a Codex reroute,
not provider-reported. Independent rollout `turn_context` recorded turn 1
`model=openai/gpt-5-mini` (default effort) and turn 2
`model=openai/gpt-5-nano effort=low`; provider-side confirmation was not checked.
See [Path to `verified`](#path-to-verified).

Deployment limit (2026-11-13): Modal's gVisor forbids the namespaces Codex's
Linux sandbox needs. `serve.py` configures `sandbox_mode = "read-only"`, which
cannot be enforced there: commands run under Codex's sandbox fail. Self-hosted
sessions are unaffected because commands run on the unsandboxed executor.
See [Modal deployment](deploy/modal/README.md).

| Area | Ops | Status | Works today | Remaining gap |
| --- | --- | --- | --- | --- |
| Saved agents | AGT-001..005 | 5 verified | Full CRUD, creation-order pagination, every pinned configuration family with omit/null/replace semantics, snapshots after deletion and cold resume, programmatic calling with catalog-mode refusals, project-scoped | None within the parity target; saved_agent passed via OpenRouter (2026-11-13). Instructions and input take up to 1 MiB and agents 128 tools |
| Sessions | SES-001..005 | 5 verified | CRUD, initial input/streaming, `none` and `self_hosted`, `vault_ids`, next-turn model/effort/tier, durable deletion, programmatic calls and automatic compaction through cold resume, project-scoped | None within the parity target; session_update passed via OpenRouter (2026-11-13), with the model/effort change independently recorded in rollout turn_context; serving model not confirmed provider-side. Every SDK create, read, and update field is handled (audited 2026-10-12) |
| Input events | EVT-001 | verified | Validated ordered batches, text up to 1 MiB, data-URL and remote https images (fetched under the egress rules), string and content-part function results (10,000 bytes), session-scoped `Idempotency-Key` replay | None. Batching and idempotency semantics are the contract by decision (2026-10-12) |
| Streaming | EVT-002 | verified | Lifecycle, item, text/reasoning, content-part, error, subagent, environment, and command-output events; initial stdout and fast-exit output arrive before item.done with matching IDs/output_index and saved output (macOS 2026-10-13 and Linux 2026-11-13 mock evidence) | None within the parity target. Live-only; best-effort limits: 10,000 deltas per call, and oldest deltas dropped on lag beyond 64 chunks before the watcher starts; aggregated output unaffected |
| Items and turns | ITM-001, TRN-001..002 | 3 verified | Message, reasoning, function, MCP, web search, command, and subagent create/send/interrupt items (with the subagent's model), and `agent_message` items between agents; `queued` turns while input waits for an executor; turn usage, error codes, and pagination | None. Engine limit: wait/close/resume subagent items |
| Subagents | SUB-001..006 | 6 verified | Resources, paginated items and turns per subagent, cancellation, restart continuity | None. `instructions` is always null and `closed` never occurs: Codex V2 reports neither, accepted as engine limits (2026-10-12) |
| Functions | (in EVT/BEH-007) | verified | Tagged functions, result/error flow, content parts, duplicate and race handling, deferred functions found through tool search (offered up front on models without it) | None |
| MCP and built-ins | (BEH-008) | verified | HTTP MCP with vault/inline authorization snapshots (write-only and encrypted), non-secret headers round trip (Authorization headers refused; use transport.authorization), request metadata in tools/call `_meta`, tool selection; built-in web search with every setting; programmatic calls (local mock evidence); real-provider web search passed via OpenRouter (2026-11-13) | None within the parity target. Three of five completed search actions had empty queries; uninvestigated Codex/provider observation |
| Harness features | (BEH-009, G08) | BEH-009 verified; G08 complete locally (2026-10-13) | Multi-agent delegation within Codex V2 limits; automatic compaction preserves full public history through cold resume; sandboxed V8 nested function calls, cancellation, fail-closed host and model-mode refusals; capability-directory skills, plugin skills, and pinned Skills API versions isolated through API/worker restart | None within the parity target; no public compaction controls/items in the pinned contract |
| Environments | ENV-001..003 | 3 verified | Self-hosted executor attachment through the rendezvous relay, `pending`/`connected`/`disconnected`/`failed` status kept across restarts, writing and listing environment files, executor loss handled apart from worker loss | None for ENV-001..003 and BEH-010 within the parity target; second real-provider self_hosted run passed, first on Linux (2026-11-13). Whether the Modal executor run counts for BEH-018 remains open. Self-hosted environments never expire |
| Environment templates | TPL-001..005 | 5 missing | `openai_hosted` returns 400 | **Out of scope** (decision 2026-10-06) |
| Artifacts | ART-001..004 | 4 verified | Routes validate their inputs and return 404 or an empty page | None; publication exists only for OpenAI-hosted environments, which are out of scope |
| Files | FIL-001..005 | 5 verified | Streaming multipart upload (50 MiB) with SHA-256, six purposes, `expires_after`, integrity-checked download, project-scoped | None; upload parts, batch processing, and range requests are **out of scope** (decision 2026-10-12) |
| Skills | SKL-001..006, SKV-001..005 | 11 verified | ZIP and directory uploads validated against the guide's limits and the Agent Skills frontmatter rules, numbered versions with default/latest pointers, integrity-checked archive download, project-scoped; `self_hosted` sessions install pinned `skill_reference` versions (local extension) | None within the parity target |
| Vaults and credentials | VLT-001..004, CRD-001..005 | 9 verified | Encrypted CRUD, rotation without ID changes, snapshots held by running sessions, HTTP MCP authentication, project-scoped | None. `environment_variable` credentials need hosted environments (out of scope); OAuth refresh and external key management are **out of scope** (decision 2026-10-12) |
| Webhooks | WHK-001..008 | 8 verified | Endpoint CRUD, every event type the SDK lists (only `agent.session.*` are produced), signed and retried deliveries that survive restarts, secret rotation with a 24h overlap, test delivery of any type; endpoints and deliveries project-scoped | None. Events carry environment data and the actual required action; delivery order is not guaranteed, as in the guide |
| Traces | TRC-001 | verified | OTLP JSON export, including `execute_tool` spans and the service's configured turn model, updated on a Codex reroute; not provider-reported | None. With no reference schema, this format is the contract (decision 2026-10-12); subagent instructions are a Codex limit |
| Projects and keys | (BEH-001, BEH-003, G13) | BEH-001 and BEH-003 verified | Project API keys with permissions (CLI-managed), 401/403/404 swept over every operation, every resource project-scoped; the `OpenAI-Beta` header is accepted and ignored everywhere (decision 2026-10-12) | None. Admin API out of scope by decision (2026-10-12); executors use project keys holding `api.environments.connect` |
| Service operation | (BEH-017) | verified | Managed supervision, fencing, reconciliation, data-directory lock, migrations, and per-project indexes; Linux/macOS spawn-to-record recovery via synced pre-spawn intent, inherited lock, and exact command/start-time checks (macOS runtime 16/16, lib 24/24; Linux recovery/streaming 43/43 three times, 2026-11-13) | None within the parity target. Lost function waiters stay unresolved/unavailable by decision (2026-10-13); failed inspection or intent lock held past 10 seconds fails startup safely. Legacy records supported; external workers untouched; executor loss is BEH-010 |

### Path to `verified`

Updated: 2026-11-13. Capability, saved-agent, session-update, and a second
self-hosted run are recorded above; BEH-017 and EVT-002 also have Linux runs.

1. **SDK evidence (G12):** expand official SDK scenarios to every implemented
   resource/capability; the corresponding checklist item remains unchecked.
2. **Provider interoperability (BEH-018):** decide whether the Modal Sandbox
   executor run satisfies this row; it stays missing/open pending that decision.

## Implementation sequence

1. **Freeze the public contract and build its test harness.** Inventory each
   operation from the reference or published SDK types, including experimental
   headers, payload variants, status/error shapes, limits and pagination. Record
   evidence and gaps per operation. Keep current routes usable during migration.
2. **Implement the session contract with no environment.** Saved-agent operations,
   inline configuration, snapshots and overrides; session operations; input events;
   normalized events; saved items and turn retrieval; function result/error flow.
   Acceptance: official SDK lifecycle and function examples run against our URL.
3. **Make the runtime service-owned.** Start and supervise Codex workers, separate
   HTTP lifecycle from execution, reconcile backend state, and support retrieving
   pending actions after a client reconnect. Test failures at each actual process
   boundary; do not mistake persisted request IDs for restorable execution.
4. **Complete capabilities.** Map documented built-in and MCP controls, deferred
   tools, output settings, delegation, programmatic tool calling and skills/plugins.
   Test both advertised capabilities and execution, including session isolation.
5. **Implement environments and artifacts.** Self-hosted executor attachment and
   connection actions, then our managed sandbox provider, setup/files/network
   settings and artifacts. Define cleanup ownership for each mode.
6. **Finish the remaining service APIs and acceptance suite.** Webhooks, vaults,
   usage/observability, provider-backed capabilities and real-provider tests.

These are reviewable milestones, not exclusions from the full parity objective.
Do not call the service functionally equivalent while inventory rows remain open.

## Recovery contract corrections

- Official event streams do not replay missed events. Recovery opens a new
  stream, retrieves current session/items, and merges buffered updates by item ID.
- Pending functions are discovered through current `required_actions`, not merely
  a function-call item in history. Return saved results using the same turn/call IDs.
- Application client restart, API worker failure, harness failure, and environment
  loss are different cases and require separate tests.
- Self-hosted environment loss can fail a tool without failing its entire turn.
  The documented API does not automatically restart a killed command or guarantee
  pending-input recovery after a process crash. Later input may request reconnection.
- Deleting a self-hosted session does not stop caller-owned compute. Model service
  ownership must remain distinct from environment ownership.

## Sources reviewed

- [Overview](https://developers.openai.com/api/docs/guides/agents-api/overview)
- [Architecture](https://developers.openai.com/api/docs/guides/agents-api/architecture)
- [Configuration](https://developers.openai.com/api/docs/guides/agents-api/configuration)
- [Run sessions](https://developers.openai.com/api/docs/guides/agents-api/sessions)
- [Manage sessions](https://developers.openai.com/api/docs/guides/agents-api/sessions/manage)
- [Events and items](https://developers.openai.com/api/docs/guides/agents-api/sessions/events)
- [Functions](https://developers.openai.com/api/docs/guides/agents-api/tools/functions)
- [Sandbox lifecycle](https://developers.openai.com/api/docs/guides/agents-api/environments/lifecycle)

## Review boundaries for the session slice

The implementation spans two dependent review units: request/configuration
translation (`contract.rs`, shared agent route selection), followed by persisted
public records and normalized events (`records.rs`, action/worker hooks). The
second unit needs the first to associate public sessions with Codex threads;
review the HTTP/SDK integration fixtures with that second unit. Neither unit
changes codex-core, provider policy, process ownership, or environment provisioning.

Reference: [session creation](https://developers.openai.com/api/reference/resources/beta/subresources/agents/subresources/sessions/methods/create).
The local SDK test does not establish real-provider execution or full API parity.

Verification for this earlier session slice: all five `codex-agents-api` tests
passed with `--run-ignored all`, including strict `openai==3.17.0` SDK validation.
The SDK exercise covers saved agents, session snapshots, function success/error, client
reconnect, item pagination, steering/cancellation, and follow-up after restarting
the API and in-process harness. The provider was mocked; remote-socket and
real-provider acceptance were not verified at that milestone; later evidence is
recorded under Current status above.
