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

## Current status

Updated: 2026-10-12. Counts come from [CONTRACT_INVENTORY.json](CONTRACT_INVENTORY.json);
per-operation detail lives in its `remaining` fields.

**Scorecard:** 67 operations: 10 `verified`, 2 `implemented`, 39 `partial`,
16 `missing`. Of the 16 missing, 5 (environment templates) are outside the
parity target by decision (2026-10-06), so **11 missing operations (Skills)
remain in scope**. Of 18 behavior rows, 16 are partial and 2 are missing
(BEH-011 is out of scope; BEH-018, provider interoperability, is open), and
3 are verified (BEH-006, BEH-012, BEH-015).

The 2026-10-12 promotion pass, backed by a full crate run (59/59), moved rows
whose only gaps were out of scope or intended behavior. The remaining partial
rows are held back for reasons that repeat: there is no project-scoped identity (the
service has a single operator-token tenant), evidence comes from mock
providers plus OpenRouter runs only, and there is no Windows evidence. See
[Path to `verified`](#path-to-verified).

| Area | Ops | Status | Works today | Remaining gap |
| --- | --- | --- | --- | --- |
| Saved agents | AGT-001..005 | 5 partial | Full CRUD, creation-order pagination, every pinned configuration family round trips with omit/null/replace semantics, snapshots survive agent deletion and cold resume | Project scope; prototype limits; built-in capability execution (G07/G08) |
| Sessions | SES-001..005 | 5 partial | CRUD, initial input and streaming, `none` and `self_hosted` environments, `vault_ids`, next-turn model/effort/tier updates, deletion with a durable cleanup queue | Project scope; complete configuration; environment actions in the session object; provider-specific model compatibility |
| Input events | EVT-001 | partial | Validated ordered batches, text and data-URL images, string and content-part function results (10,000 bytes), session-scoped `Idempotency-Key` replay | Remote image URLs; batching and idempotency are local decisions with no upstream contract |
| Streaming | EVT-002 | partial | Lifecycle, item, text and reasoning deltas, content parts, `error`, subagent, environment, and command-output events | Codex sends no command-output deltas for remote commands (output arrives on the item); no replay, by contract |
| Items and turns | ITM-001, TRN-001..002 | 1 verified (TRN-002), 2 partial | Message, reasoning, function, MCP, and subagent create/send/interrupt items; turn usage, error codes, and pagination | Web search, command, wait/close/resume subagent, and `agent_message` items; `queued` turn state for worker-started turns |
| Subagents | SUB-001..006 | 5 verified, 1 partial (SUB-001) | Resources, paginated items and turns per subagent, cancellation, restart continuity | `instructions` is always null and `closed` never occurs (Codex V2 reports neither) |
| Functions | (in EVT/BEH-007) | partial | Tagged functions, result/error flow, content parts, duplicate and race handling | Deferred functions and tool search (G06) |
| MCP and built-ins | (BEH-008) | partial | HTTP MCP with vault credentials and tool/resource selection | Inline MCP authorization, request metadata, web search, per-capability built-in slices (G07), programmatic tool calling (G08) |
| Harness features | (BEH-009) | partial | Multi-agent delegation | Compaction controls, programmatic tool calling, and skills/plugins selection (G08) |
| Environments | ENV-001..003 | 2 implemented, 1 partial (ENV-001) | Self-hosted executor attachment through the rendezvous relay, `pending`/`connected`/`disconnected` status, writing and listing environment files, executor loss handled apart from worker loss | `expired`/`failed` statuses (ENV-001); one real-provider run; Windows and cross-OS execution |
| Environment templates | TPL-001..005 | 5 missing | `openai_hosted` returns 400 | **Out of scope** (decision 2026-10-06) |
| Artifacts | ART-001..004 | 4 verified | Routes validate their inputs and return 404 or an empty page | None; publication exists only for OpenAI-hosted environments, which are out of scope |
| Files | FIL-001..005 | 5 partial | Streaming multipart upload (50 MiB) with SHA-256, six purposes, `expires_after`, integrity-checked download | Single tenant; no upload parts, batch processing, or range requests; `status` is always `processed` |
| Skills | SKL-001..006, SKV-001..005 | **11 missing** | Nothing | Whole resource: ZIP upload, metadata, versions, archive download, and `skill_reference` wiring into sessions |
| Vaults and credentials | VLT-001..004, CRD-001..005 | 9 partial | Encrypted CRUD, rotation without ID changes, snapshots held by running sessions, HTTP MCP authentication | `environment_variable` credentials; OAuth refresh (rejected); project scope; external key management |
| Webhooks | WHK-001..008 | 8 partial | Endpoint CRUD, signed and retried deliveries that survive restarts, secret rotation with a 24h overlap, test delivery | Only the five `agent.session.*` event types; environment data in events; ordering after a failed attempt |
| Traces | TRC-001 | partial | OTLP JSON export, including `execute_tool` spans | No reference schema or SDK method exists; per-response model and subagent instructions are not recorded |
| Service operation | (BEH-017) | partial | Managed worker supervision with backoff, reconnect fencing, turn/function reconciliation, Unix orphan reclaim, data-directory lock, migrations, Modal deployment | Windows orphan containment; the spawn-to-record window; durable identity and list indexes (G02) |

### Path to `verified`

Ordered by the number of rows each item unblocks:

1. **Projects, API keys, and permissions ([G13](GOALS.md#g13--projects-api-keys-and-permissions)).**
   This is the blocker cited by nearly every agent, session, vault, file, and
   webhook row. Decided 2026-10-12: the service is multi-project (keys belong
   to projects; other projects' resources return 404; missing permissions
   return 403), with CLI-managed projects and keys for now.
2. **Second promotion pass.** The first pass (2026-10-12) promoted artifacts,
   subagent lists, and turn lists. Once item 1 is settled, files (FIL-002..005),
   webhooks, vaults, and the agent/session CRUD rows are the next candidates.
3. **Skills API (11 ops).** This is the largest in-scope block that is still
   missing, and G08 skills/plugins selection depends on it.
4. **Capability gaps:** deferred functions and tool search (G06), built-in
   capability slices (G07), compaction and programmatic tool calling (G08).
5. **Platform evidence (G12):** Windows and cross-OS worker/executor runs, SDK
   scenarios for every implemented resource, and
   provider interoperability (BEH-018).

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

Verification for this slice: all five `codex-agents-api` tests passed with
`--run-ignored all`, including strict `openai==3.17.0` SDK validation. The SDK
exercise covers saved agents, session snapshots, function success/error, client
reconnect, item pagination, steering/cancellation, and follow-up after restarting
the API and in-process harness. The provider was mocked; remote-socket and
real-provider acceptance remain unverified.
