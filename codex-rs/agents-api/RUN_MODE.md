# Run mode

Run mode makes one `codex-agents-api` process the runtime of a single swarm
run: a team of agents working together for one developer, on one host, for
the length of that run. The control plane boots the runtime from the run's
configuration, the agents' sessions run on it, and when the run ends it
exports everything and exits. Nothing in the runtime outlives the run.

The service as it is today is a long-lived service. It has one Codex worker
for every session, keeps sessions across restarts, and cleans them up one by
one. Run mode keeps all of that working and adds what a per-run runtime
needs. It is opt-in: without its flags the service behaves exactly as before.

## Why

The swarm's per-run runtime spike (swarm-agensh `docs/run-api.md`, "Spike
results", 2026-11-18) ran each agent on its own `agents-api` with its own
worker. Teams of 2 and 8 agents passed their acceptance tests. What the
spike showed:

- **One worker per agent.** With one worker for the whole team, one agent's
  trouble stalls the others. `b3733495cb` fixed one such stall, where one
  unreachable executor held up every session's thread operations. A worker
  per agent also lets a crashed worker restart without the other agents
  noticing.
- **Memory.** Each worker took 260–325 MB. The `agents-api` service and the
  environment provider took about 130 MB per agent between them. In run mode
  there is one service and one provider for the whole run, so that part
  becomes a fixed amount per run. At 24 agents the workers need about 7–8 GB.
  That fits a 32 GB host, so v1 has no need to pack agents into fewer workers.
- **Starting together costs memory.** When 8 workers started at the same
  moment, each briefly reached about 570 MB. Run mode should stagger worker
  starts on large teams.

## What run mode covers

| Part | What it does | Increment |
| --- | --- | --- |
| Worker pool | Several managed Codex workers, each session assigned to one; staggered starts; each worker restarted on its own; clean shutdown | 1 (this change) |
| Run config | Boot from one run document (team size, provider, model, key, budget) instead of flags and a hand-written `config.toml` | 2 |
| Run budget and usage | Add up every session's tokens, subagents included, against `budget.model_tokens`; stop the run when it is spent; report usage live | 3 |
| One event stream | Every session's events, tagged with their worker, on one stream for the control plane | 3 |
| Export and exit | At the end of the run, write every session, turn, item and trace to object storage, then exit | 4 |
| Control-plane environments | An environment provider that asks the control plane for agent sandboxes instead of calling Modal | 4 |

## Increment 1: the worker pool

### Turning it on

`--workers N` (from 1 to 64; the default is 1) starts N managed workers. With
1, nothing changes: the service starts and supervises a single worker, as
before. `--workers` cannot be combined with `--app-server-socket`, because
the service does not own an external worker and cannot start more of them.

`--worker-start-stagger-ms MS` (default 1000) is the delay between one
worker's start and the next. Each worker finishes its startup handshake
before the next one starts, and the delay comes after that. Startup memory
peaks are then not stacked on top of each other. The service starts taking
requests once every worker is connected, so sessions are spread across the
whole pool from the start.

### Worker homes

Every worker has its own `CODEX_HOME`, because a home holds one worker's
rollouts and is locked by that worker. Worker 0 uses the configured home
(`--codex-home`, by default `DATA_DIRECTORY/codex-home`). This is the same
home a single worker uses, so a data directory can switch from one worker to
a pool and its existing threads stay where they are. Worker `i > 0` uses
`<home>-worker-<i>` next to it, for example
`DATA_DIRECTORY/codex-home-worker-3`.

The operator configures worker 0's home as before. Before each start of
another worker, the service copies `config.toml` from worker 0's home into
that worker's home, so every worker of the run runs with the same
configuration. Keys come from the environment, which every worker inherits.
Like a single worker, no worker inherits the service's own secrets.

### Assigning sessions to workers

A session is assigned to a worker when it first needs a thread, that is, on
its first input. The service picks the connected worker with the fewest
sessions assigned to it, and the lowest-numbered one on a tie. With as many
workers as agents, every agent's session gets a worker to itself.

The assignment is stored in a new table, `session_workers`, in the same
statement that chooses it. Two sessions starting at the same moment
therefore cannot both take the same free worker, and the assignment holds
across worker restarts and service restarts. Deleted sessions stop counting
towards their worker's load.

Every call the service makes to Codex goes to the worker that owns the
thread: the session's own thread, its subagents' threads, and its thread
deletion after the session is deleted. Calls that do not name a thread
(`model/list`, `config/read`) go to the lowest-numbered connected worker;
the workers share one configuration, so any of them can answer. A
self-hosted environment is added only to the worker of the session that
uses it, and removed from every worker that had it.

A session with no assignment belongs to worker 0. That covers the threads
of a data directory that ran with one worker before it switched to a pool.

### Supervision and failures

Each worker is supervised on its own. When a worker exits, its supervisor
reaps it, respawns it in the same home with the same backoff as today (five
attempts, starting at 200 ms, doubling up to 5 s), and reattaches it. While
it is down:

- only that worker's sessions are affected. Their mutations return 503, their
  pending function calls become unavailable, and their interrupted turns
  fail or are continued exactly as with a single worker;
- only their live streams end. A public event stream
  (`GET /v1/agents/sessions/{id}/events`) ends only if its session is on the
  lost worker;
- every other worker's sessions keep running, with their streams open.

`GET /healthz` returns 200 only while every worker is connected. With a pool,
it also lists each worker as `{"index": i, "status": "connected"}` or
`"disconnected"`.

If a worker's restarts are exhausted, the process exits, as it does with one
worker. Five failed restarts in a row point to a broken binary, home or
configuration, which the other workers would hit too, and a supervisor
outside the service is better placed to decide what to do with the run.

Startup reports each worker on stderr as
`agents-api managed worker <i> pid=<pid>`. A single worker keeps the line
`agents-api managed worker pid=<pid>`, which existing supervisors parse.

### Shutdown

On SIGTERM or Ctrl-C the service stops taking requests, closes every worker
connection, and then shuts all workers down at the same time. Each worker
gets the existing 10-second grace period before it is killed and reaped, so
the whole pool stops within about 10 seconds whatever its size.

### Not in this increment

- The run config document. Workers are still configured through worker 0's
  `config.toml` and the environment.
- Moving a session from a worker that cannot be restarted. A session stays on
  its worker for life.
- Shrinking a pool. A data directory restarted with fewer workers cannot
  serve the sessions of the workers it no longer runs.
- Sharing one worker between agents by design. It happens only when there
  are more sessions than workers.
- Run-wide budget, the merged event stream, export on exit, and the
  control-plane environment provider.

### Tests

- Library tests run `AgentsApi::with_workers` over two in-process workers.
  They check that two sessions get different workers, each with its thread in
  its own worker's home, and that both complete turns. They also check that
  replacing one worker's connection leaves the other session working and
  ends only the lost worker's public streams.
- A CLI test starts `codex-agents-api --workers 2`. It checks that both workers
  start, that sessions spread across them, that killing one worker leaves the
  other session serving while the killed one is restarted and resumes, and
  that SIGTERM stops the API and both workers.
- The existing single-worker tests stay as they are and must keep passing.
