# Harness Capability Map

A coding-agent harness is the runtime around the model that turns user intent
into controlled, inspectable workspace changes. It owns the task contract,
instructions and context, model and tool execution, change integrity,
verification, recovery, interfaces, security, and the evidence used to improve
all of them.

This is a current-state capability map, not a feature list or backlog.
`PLAN.md` sequences planned work; this file defines the surface that must not be
forgotten.

## Assessment rules

- **Score the supported end-to-end path.** Today that is `dex` → daemon →
  HTTP/SSE TUI. Code not wired into that path, or available only in local
  one-shot mode, is called out but does not earn a full score.
- **Evidence beats intent.** Prompts, comments, and README claims are not
  harness guarantees. A capability counts when the active runtime enforces it.
- **Trust uses the weakest exposed path.** A safe file tool does not make an
  unrestricted shell safe; a durable JSONL file does not make daemon resume
  work; remote transport is not collaboration.
- **No aggregate score.** A zero in authorization, containment, verification,
  or recovery can block a release regardless of strengths elsewhere.

## Maturity scale

| Level | Meaning |
|-------|---------|
| **0** | Absent, unsafe for the stated use, or not wired into a supported path |
| **1** | Partial/happy-path; relies materially on the model, user, or manual recovery |
| **2** | End-to-end harness behavior with explicit failure handling and tests |
| **3** | Measured, resilient, policy-ready, and protected by behavioral regressions |

A level-3 claim must name a runnable acceptance check, eval, or service-level
objective. “Best in class” without evidence is not a maturity level.

## Board

`P0`–`P5` refer to phases in `PLAN.md`; “partial” means that phase covers only
part of the area. `—` means the important gap is not currently planned.

| # | Area | Pillar | Now | Planned |
|---|------|--------|-----|---------|
| 1 | Objective, acceptance & progress | Direction | **1** | P1 partial ✓ |
| 2 | Turn orchestration & stop semantics | Direction | **2** | — |
| 3 | Failure recovery & re-planning | Direction | **2** | P4 ✓ |
| 4 | Instruction hierarchy & behavior policy | Knowledge | **1** | — |
| 5 | Context lifecycle | Knowledge | **2** | P3 ✓ |
| 6 | Repository & environment intelligence | Knowledge | **1** | P2/P3 partial (P2 verify ✓, repo map still pending) |
| 7 | Memory & session continuity | Knowledge | **1** | P0 partial ✓ (plan restore ✓, registry still in-memory) |
| 8 | Tool protocol & reliability | Action | **2** | — |
| 9 | Change control & artifact integrity | Action | **1** | — |
| 10 | Verification & completion evidence | Action | **1** | P2 ✓ |
| 11 | Parallelism & delegation | Action | **1** | Deferred |
| 12 | Authorization, approvals & audit | Trust | **2** | P6 ✓ |
| 13 | Isolation & resource containment | Trust | **1** | — |
| 14 | Secrets, privacy & remote access security | Trust | **0** | P7 |
| 15 | Durability & effect recovery | Trust | **1** | — |
| 16 | Interactive UX & human control | Interface | **2** | P1–P3 partial ✓ (goal/plan+verify in bar) |
| 17 | Automation & protocol contracts | Interface | **1** | — |
| 18 | Extensibility & integrations | Interface | **1** | — |
| 19 | Model/provider portability | Platform | **2** | P5 ✓ |
| 20 | Observability, usage & cost | Platform | **1** | — |
| 21 | Performance & token economy | Platform | **2** | P3/P5 partial ✓ |
| 22 | Quality evaluation | Platform | **1** | — |
| 23 | Operations, configuration & compatibility | Operations | **1** | — |
| 24 | Remote use & collaboration | Operations | **1** | — |
| 25 | Long-running autonomy | Operations | **0** | Deferred |

---

## Pillar A — Direction

### 1. Objective, acceptance & progress — **1**

**Owns:** the durable task contract: goal, constraints, acceptance criteria,
plan, current step, completion state, and the user's ability to correct them.

- **Evidence now:** `core/types::Plan{goal,steps}` persisted via `session.set_state("plan",json)` (`ui/slash` `/goal` `/plan add|done|clear` + `session::load_plan` + `session::save_plan`), re-injected every turn as `system name:plan` (`agent/loop::plan_injection` + `persist_pending`), synced to remote via `SinkLine::Plan`/`StreamEvent::Plan` (`ui/remote`, `daemon/server`, `protocol::ChatRequest.plan`, `client/http::ChatOptions.plan`), shown as `Goal:…·Plan 3/7` in `ui/render::plan_status`/`footer_text`, restored on `/resume` (`ui/slash::apply_session_state` reads `plan` key). Acceptance `cargo test --all-targets plan_is_injected_before_first_model_call`.
- **Material gap:** still no `constraints`/`acceptance criteria` ledger or harness stop condition linking completion to evidence. `P1` shipped goal+steps only; constraints + acceptance evidence remain partial. Resource/turn budgets are not part of the task contract.
- **Level 3 bar:** task state survives compaction, process restart, and client
  reconnect; the user can edit it; completion links every acceptance criterion
  to a result or an explicit waiver.

### 2. Turn orchestration & stop semantics — **2**

**Owns:** model/tool iteration, event ordering, budgets, deadlines,
cancellation, backpressure, and clean terminal outcomes.

- **Evidence now:** `agent/loop.rs` provides iteration, prompt-token, and
  elapsed-time limits; streaming model calls; tool dispatch; a wrap-up nudge;
  and cancellation polling. The daemon permits one active turn per session and
  emits terminal SSE events.
- **Material gap:** the turn deadline is checked between model rounds, not
  across every blocking boundary. Approval waits and retry sleeps have no
  shared deadline; an SSE disconnect does not define cancel-or-continue and
  reattach behavior. A mixed batch is wholly serialized when any call mutates.
- **Level 3 bar:** one deadline and cancellation contract covers provider
  calls, retries, approvals, tools, and event delivery; disconnects and panics
  end in a deterministic terminal state; chaos tests prove no wedged turn.

### 3. Failure recovery & re-planning — **2**

**Owns:** recognizing that an approach is failing, changing strategy, and
escalating to the user instead of looping.

- **Evidence now:** provider retry/backoff, structured tool failures, malformed
  call handling, repeated-successful-call blocking, late wrap-up nudge, plus `P4` ledger in `agent/loop.rs`: `last_failed` (identical failed `cache_key` ≥3), `edit_paths` (same path 3×), `search_streak` (`grep`/`find` ≥4 without `read`), `last_verify_hash` (first-line hash of `verify` failure unchanged), `escalation_count` capped 3 via `stuck_nudge` 1:direct fix 2:arch 3:question assumption 4:re-plan → abort `stuck:…escalation limit` . Verified by code path, not yet by `stuck-agent` eval suite.
- **Material gap:** ledger is per-turn in-memory only, not a durable `failure ledger`; no staged `re-plan` object, no eval harness proving `1→2→3→re-plan` terminates with preserved partial work. `Level 3` eval still missing.
- **Level 3 bar:** deterministic failure signatures trigger bounded escalation;
  recovery attempts are visible; scripted stuck-agent evals terminate with a
  useful re-plan and preserved partial work.

---

## Pillar B — Knowledge

### 4. Instruction hierarchy & behavior policy — **1**

**Owns:** how system policy, project instructions, skills, user requests, and
retrieved content are ordered, bounded, attributed, and inspected.

- **Evidence now:** `llm/prompt.rs` builds a static system prompt, injects the
  first `AGENTS.md`/`CLAUDE.md` found while walking upward, and advertises
  discovered skill names/descriptions.
- **Material gap:** precedence and provenance are implicit; project instruction
  size is unbounded; there is no effective-prompt inspection or explicit
  instruction/data boundary. The active remote TUI cannot load a skill body
  with `/skill:<name>`, although the prompt advertises it.
- **Level 3 bar:** deterministic precedence, source labels, token budgets, and
  policy tests cover conflicting and malicious repository instructions; users
  can inspect exactly which instructions affected a turn.

### 5. Context lifecycle — **2**

**Owns:** selecting, ordering, compressing, invalidating, and restoring what the
model sees at each call.

- **Evidence now:** full session replay, usage/estimate-driven summarization,
  recent tool-call/result pair protection, output caps, bounded read/search
  tools, shell stitching, plus `P3` `agent/loop::turn_start_context` (once/turn `system name:context` = `Plan` summary + `git status --short` + `git diff --stat` via `tool_git`, capped 10 lines, silent outside repo) and per-turn `plan_injection`, and `agent/compaction::summarize_old_messages` now preserves `goal/plan` verbatim + recent `name:verify` failures. Test `plan_is_injected_before_first_model_call` asserts `name:context`+`name:plan` present.
- **Material gap:** still no provenance-bearing `context manifest` of relevant files/decisions/failures, no stale-fact invalidation, no eval tracking answer quality/tokens across growth. Manifest + invalidation remain for `Level 3`.
- **Level 3 bar:** a provenance-bearing context manifest is rebuilt per turn,
  stale facts are invalidated, required task facts survive compaction, and evals
  track answer quality and tokens as sessions grow.

### 6. Repository & environment intelligence — **1**

**Owns:** the project map and execution environment: languages, toolchains,
modules, conventions, dependency shape, build/test commands, and change-aware
refresh.

- **Evidence now:** project instruction discovery; `read`, `grep`, `find`, and
  read-only `git`; plus branch/dirty state in the UI, plus `P2` explicit `verify_command` (no auto-detection) and `P3` turn-start `git status/diff --stat` snapshot. The model still discovers the rest manually.
- **Material gap:** no cached repo map, toolchain or package-manager detection, *auto* verification-command discovery, symbol/diagnostic index, or environment readiness check. The workspace is still the daemon process's current directory.
- **Level 3 bar:** a small inspectable map is built once, invalidated by actual
  changes, and supplies correct build/test commands and navigation with less
  re-exploration than the baseline.

### 7. Memory & session continuity — **1**

**Owns:** what conversation, task, decision, and preference state survives
turns, compaction, reconnects, and new processes.

- **Evidence now:** append-only session files and replay exist; local one-shot
  mode can continue a supplied session path; state, clear, and rename record
  types exist. `P0` wired: `session::load_session_state` + `load_plan`/`save_plan` (`plan` JSON via `session_state`), `ui/slash::apply_session_state` restores `model`+`provider`+`plan` on `/resume` (one-shot/local), remote plan sync via `ChatRequest.plan`/`ChatOptions.plan` persisted on daemon. Test `apply_session_state_restores_model` + `plan_is_injected`.
- **Material gap:** the default daemon keeps its session registry only in
  memory, always creates a new TUI session, and cannot attach to a persisted
  session after restart. `/resume`, `/name`, `/provider`, and `/skill` are still not supported by the active *remote* path (local/one-shot only). Daemon registry not rebuilt; no explicit project memory.
- **Level 3 bar:** all active paths restore the same task/model/skill/session
  state after restart; remembered facts are user-visible, editable, scoped,
  attributable, and removable.

---

## Pillar C — Action

### 8. Tool protocol & reliability — **2**

**Owns:** model-facing schemas, argument validation, result/error contracts,
timeouts, output shaping, cancellation, and truthful side-effect metadata.

- **Evidence now:** bounded `read`, `grep`, `find`, `git`, and `chain`; `bash`
  with timeout/output capture/process-group kill; exact/fuzzy `edit`; `write`;
  structured internal errors; malformed-call handling; and read/mutate/
  permission metadata. Independent read-only calls can execute concurrently.
- **Material gap:** truncation and fan-out limits are not consistently
  machine-readable, the search stack assumes host Unix tools, and metadata is
  not yet a proved side-effect contract. Symbol navigation or diagnostics
  should be added only for demonstrated task failures.
- **Level 3 bar:** contract/property tests cover every argument, truncation,
  cancellation, and side-effect claim; failures always prescribe a valid next
  action; no result is silently incomplete.

### 9. Change control & artifact integrity — **1**

**Owns:** preserving user work while applying, reviewing, grouping, reverting,
and handing off agent changes.

- **Evidence now:** unique-match edits, workspace path checks for file tools,
  serialized mutation batches, and read-only git status/diff inspection.
- **Material gap:** writes are not transactional or guarded by an expected file
  version; there is no before/after hash ledger, patch preview before approval,
  baseline separating pre-existing changes, checkpoint/undo, rollback, or
  enforced final diff review. Shell mutations bypass file-level accounting.
- **Level 3 bar:** every mutation belongs to a reviewable change set with
  preconditions and before/after evidence; concurrent user edits are never
  overwritten silently; cancellation can reconcile or roll back partial work.

### 10. Verification & completion evidence — **1**

**Owns:** deciding which checks are relevant, running them after changes,
feeding failures back, and defining “done.”

- **Evidence now:** `P2` dirty-state hook: `ToolState::verify_dirty` set on `is_mutating` batch, runs `verify_command` (`DEX_VERIFY`/`config.sample.json`) via `tools::execute_outcome("bash")` (same timeout/caps/cancel), deduped (skip if no mutation). Pass → `SinkLine::System "verify ✓"`/`StreamEvent::System`, fail → `user name:verify` tail (hash of first line for `stuck` dedup) fed to next model call. Prompt asks model to run checks. One test path exists (`ToolThenAnswer`); no auto-detection yet.
- **Material gap:** no auto-detection of `cargo test`/`go test`/etc., no `pass/fail` event/artifact gate, no `waived/not applicable` state, no enforced final diff review. `verify_command` must be set manually.
- **Level 3 bar:** every change has a verification disposition; relevant
  build/test/lint/diagnostic checks run against the final state; failures return
  to the loop; the final response links claims to recorded results.

### 11. Parallelism & delegation — **1**

**Owns:** dependency-aware concurrent work, conflict isolation, subtask budgets,
and synthesis.

- **Evidence now:** independent read-only calls in one model response run in
  parallel. Mutating batches are serialized under a process-global lock, and
  separate daemon sessions may run concurrently.
- **Material gap:** no dependency DAG or normalized path locking; any mutation
  serializes its whole batch; there are no subagents, worktrees, delegated
  budgets, result contracts, or merge/review step.
- **Level 3 bar:** separable subtasks show lower wall time without reducing eval
  pass rate; each worker is isolated, budgeted, cancellable, and merged through
  the same verification and change-control gates.

---

## Pillar D — Trust

### 12. Authorization, approvals & audit — **2**

**Owns:** who may request an effect, what may be done, approval scope and
expiry, policy enforcement, and an attributable decision record.

- **Evidence now:** four permission modes, TUI allow-once/allow-session/deny,
  daemon-side approval parking keyed by `Console::approval_key(name,input)` (`write`/`edit`→path hash, `bash`→command hash, else input hash), per-turn expiry (console drop), best-effort `audit.jsonl` + daemon `approve` audit with `actor`/`request_id`/`input_hash`/`decision`, and **daemon permission ceiling**: `daemon_perm.permissiveness()` from file/env is max, `ChatRequest.permission` may only be stricter (`read-only 0 < ask-writes 1 < ask-shell 2 < trusted 3`), escalation returns `403` (`daemon/server::run_turn_inner`). Tested by `cargo test` (63) + manual `DEX_PERMISSION=ask-writes` client `trusted` → `permission escalation denied`.
- **Material gap:** approvals still lack computed `diff` + policy rules (e.g. `diff` hash for `edit`), expiry is only per-turn not 10m wall-clock, actor is `local`/`remote` not token-principal, audit is best-effort not tamper-evident.
- **Level 3 bar:** the daemon owns a non-bypassable permission ceiling; clients
  may only request equal or stricter policy unless authorized; approvals are
  scoped to concrete paths/commands/diffs with expiry; every decision has an
  actor and tamper-evident record.

### 13. Isolation & resource containment — **1**

**Owns:** the blast radius of tools across filesystem, process, network,
environment, CPU, memory, disk, and time.

- **Evidence now:** direct file tools canonicalize paths and reject symlink
  escapes; shell output/time are bounded and descendants are killed as a Unix
  process group; `chain` rejects mutating tools.
- **Material gap:** `bash` runs as the daemon user with inherited environment
  and unrestricted filesystem, process, and network access. Starting in the
  workspace is not workspace confinement. There are no CPU, memory, disk, PID,
  or egress limits.
- **Level 3 bar:** untrusted turns execute in a tested sandbox with explicit
  mounts, environment allowlists, network policy, quotas, and reliable process
  teardown; trusted escape hatches are conspicuous and auditable.

### 14. Secrets, privacy & remote access security — **0**

**Owns:** daemon authentication, transport security, credential boundaries,
redaction, data retention, provider egress, and session ownership.

- **Evidence now:** provider credentials can come from environment/config,
  Responses requests set `store: false`, tool caching is opt-in, and binding to
  all interfaces prints a warning.
- **Material gap — release blocker for remote use:** the HTTP daemon has no
  authentication or TLS. An unauthenticated chat request can override both
  `base_url` and permission, combining daemon credential exposure risk with
  trusted shell execution. Sessions, tool arguments, write contents, and
  provider error bodies are stored in plaintext logs without redaction,
  retention policy, or explicit restrictive file modes.
- **Level 3 bar:** authenticated principals and session ownership are enforced;
  provider endpoints and permission ceilings are server policy; remote traffic
  uses a trusted secure channel; secrets never enter prompts/logs by default;
  retention/export/delete behavior is testable.

### 15. Durability & effect recovery — **1**

**Owns:** reconstructing conversation and side-effect state after crashes,
partial writes, disconnects, and retries without duplicating effects.

- **Evidence now:** versioned append-only JSONL, incremental message appends,
  malformed-line tolerance, clear markers, and one active turn per session.
- **Material gap:** daemon turns record `turn_start` but not durable
  `turn_complete`/`turn_failed`; the daemon registry is not rebuilt; persistence
  errors are commonly ignored. A mutation can complete before its tool call and
  result are durably recorded, so restart cannot distinguish not-started,
  completed, and unknown effects. Approval and compaction recovery invariants
  are untested.
- **Level 3 bar:** intent, approval, effect start, result, and final artifact
  hashes form a recoverable journal; restart reconciles unknown effects and
  cannot repeat a non-idempotent action silently; session migrations and
  corruption recovery are tested.

---

## Pillar E — Interface

### 16. Interactive UX & human control — **2**

**Owns:** showing intent, progress, actions, approvals, outcomes, and giving the
user timely steer/cancel/correct controls without exposing hidden reasoning.

- **Evidence now:** streaming markdown TUI, tool summaries/previews/durations,
  live usage, approval overlay, cancellation, scroll/history/autocomplete,
  branch/dirty status, terminal cleanup, plus `P1–P3` `Goal:…·Plan 3/7` in `ui/render::plan_status`/`footer_text` + `plan`/`context` system messages + `verify ✓`/`name:verify` feedback. Remote `SinkLine::Plan`→`StreamEvent::Plan` keeps bar in sync.
- **Material gap:** the active daemon path still does not wire mid-turn `steering`/`follow-ups` — `process_turn` `steering_rx=None` in `daemon/server` and Enter ignored while busy — despite dormant fields/docs. Several slash commands (`/resume` with args, provider auto) still rejected remotely; accessibility not proven.
- **Level 3 bar:** the UI truthfully shows task/step, current action, queued
  input, change set, and verification; reconnect preserves control; all help
  text is exercised against the active path; accessibility is keyboard-complete
  and does not depend on color alone.

### 17. Automation & protocol contracts — **1**

**Owns:** headless use, stable machine I/O, exit semantics, event schemas,
reconnect/replay, and CI integration.

- **Evidence now:** local one-shot prompting, JSON-lines raw tool mode,
  one-shot `run <tool>`, and daemon HTTP/SSE with health, chat, approval, and
  cancel endpoints.
- **Material gap:** no versioned public API or final-result JSON contract;
  events have no sequence/replay mechanism; clients cannot reattach to a
  persisted session; disconnect behavior and idempotency are undefined. CI
  cannot consume a structured task/change/verification result.
- **Level 3 bar:** a versioned protocol has compatibility tests, deterministic
  exit codes, resumable event streams, idempotency keys, and one structured
  final record containing outcome, changes, checks, usage, and unresolved work.

### 18. Extensibility & integrations — **1**

**Owns:** adding instructions, policies, lifecycle automation, tools, and
external systems without forking the harness.

- **Evidence now:** deterministic skill discovery with frontmatter metadata and
  configurable skill directories.
- **Material gap:** skill bodies cannot be loaded on demand through the default
  daemon/TUI path. There are no pre/post-tool or post-turn hooks, custom-tool
  registration, MCP seam, extension capability declarations, or extension
  isolation/versioning.
- **Level 3 bar:** a small versioned extension contract covers lifecycle,
  permissions, cancellation, errors, and testing; external tools cannot bypass
  authorization or observability; plain `SKILL.md` remains the zero-code path.

---

## Pillar F — Platform

### 19. Model/provider portability — **2**

**Owns:** wire adaptation, capabilities, authentication, streaming/tool-call
normalization, limits, retries, fallback, and model switching.

- **Evidence now:** `ModelClient` boundary, Chat Completions/Responses translators, OpenCode/Codex credential paths, streaming, retry/backoff, usage extraction, configurable reasoning effort, plus `P5` static `llm/config::provider_default_context_window` + `llm/client::discover_capabilities` table (streaming/tools per-provider honest, no probing) driving `context_window/2` compaction without manual config. `LlmConfig::from_env` falls back to provider default.
- **Material gap:** context limits still static not probed, model quirks manual, provider coverage OpenAI-shaped, retry pacing ignores `Retry-After` and has cancellable sleep but no fallback policy.
- **Level 3 bar:** data-backed capabilities and conformance fixtures normalize
  supported providers; switching models preserves harness semantics; fallback
  is explicit, bounded, and measured rather than silently changing behavior.

### 20. Observability, usage & cost — **1**

**Owns:** correlated traces for model calls, tools, approvals, tokens, latency,
cost, failures, and final outcomes—without recording chain-of-thought or
secrets.

- **Evidence now:** live prompt-token usage, tool durations in the UI, a tool
  audit JSONL, and provider error JSONL.
- **Material gap:** no turn/task correlation trace, completion-token or monetary
  cost accounting, cache/retry metrics, redaction, log rotation, or queryable
  outcome view. Current logs cannot reliably answer what one task cost or why
  it failed.
- **Level 3 bar:** redacted spans share task/turn/call IDs and record timing,
  usage, price, approvals, effects, checks, and outcome; budgets and regressions
  are visible without reconstructing raw logs.

### 21. Performance & token economy — **2**

**Owns:** latency, model round trips, prompt/tool-output size, cache correctness,
resource use, and cost per successful task.

- **Evidence now:** batched read guidance, concurrent read-only calls, bounded
  fan-out, `chain` to reduce model round trips, `$DEX_BIN` shell-side
  distillation, output clamps, summarization, opt-in tool caching, plus `P3/P5` honest per-provider `context_window` driving `compact_history` thresholds (`est > context_window/2`) without manual `DEX_CONTEXT_WINDOW`.
- **Material gap:** still no task-level benchmark tying improvements to success/latency/cost. Context selection transcript+plan+git only, compaction costs a model call, cache correctness not strong enough to enable by default for all searches.
- **Level 3 bar:** fixed evals report p50/p95 latency, calls, tokens, and cost per
  successful task; optimizations must preserve quality and correctness; cache
  invalidation is proved before default enablement.

### 22. Quality evaluation — **1**

**Owns:** proving that prompt, context, tool, loop, safety, and UX changes make
real tasks better rather than merely different.

- **Evidence now:** CI runs formatting, unit tests, Clippy, and dependency
  audit. In-crate tests cover useful mechanics, especially tools and rendering.
- **Material gap:** there is no seeded coding-task suite, behavioral harness
  test across daemon/API/session recovery, live-model smoke set, safety/adversary
  suite, or tracked quality/cost baseline. The declared `network-tests` feature
  currently adds no tests, and infrastructure tests do not measure agent
  success.
- **Level 3 bar:** deterministic scenarios plus a small live-model set gate
  relevant changes on task success, safety, latency, and cost; failures retain
  replayable traces and accepted score changes are explicit.

---

## Pillar G — Operations

### 23. Operations, configuration & compatibility — **1**

**Owns:** startup/shutdown, readiness, config validation, deployment, quotas,
upgrades, protocol/session migration, supportability, and platform scope.

- **Evidence now:** JSON/environment configuration, a health endpoint,
  startup readiness polling, localhost-by-default binding, configurable limits,
  and CI.
- **Material gap:** no authenticated deployment profile, graceful daemon
  lifecycle, config schema/version, API compatibility policy, session migration
  path, log retention, per-user/turn quotas, or restart reconstruction. Unix
  process-group behavior is assumed rather than declared as a platform limit.
- **Level 3 bar:** validated configuration and safe defaults fail closed;
  supervised restart preserves supported sessions; upgrades pass protocol and
  migration tests; health, capacity, and resource limits are observable.

### 24. Remote use & collaboration — **1**

**Owns:** operating away from the workspace host and, separately, sharing
sessions, control, and review among people.

- **Evidence now:** daemon/client separation over HTTP+SSE, remote streaming,
  approvals, cancellation, and multiple independent daemon sessions.
- **Material gap:** remote use is blocked on #12–#14. There is no persisted
  attach/replay, user identity, ownership, shared-session concurrency model,
  handoff, review link, or multi-client control of one session. Transport alone
  is not collaboration.
- **Level 3 bar:** authenticated users can discover and reattach to owned/shared
  sessions, replay missed events, transfer control, and review the same change
  and verification record without races.

### 25. Long-running autonomy — **0**

**Owns:** durable queued work lasting hours or restarts, bounded unattended
execution, checkpoints, verification cadence, and user check-ins.

- **Evidence now:** only per-turn budgets and a daemon process; there is no
  durable task scheduler or resumable autonomous workflow.
- **Material gap:** no queue, leases, checkpoint policy, restart continuation,
  unattended approval policy, progress heartbeat, or spend/resource budget.
  Building this before #1, #3, #10, and #12–#15 would amplify drift and risk.
- **Level 3 bar:** restartable tasks make monotonic, verified progress under
  explicit time/cost/effect budgets; risky boundaries pause for a user; stalled
  tasks stop and report rather than improvising indefinitely.

---

## Cross-cutting release gates

1. **Remote gate:** #12 authorization, #13 containment, and #14 remote security
   must be solid before non-local daemon use is presented as safe.
2. **Correctness gate:** #1 task acceptance, #9 change integrity, and #10
   verification define whether “done” means anything.
3. **Recovery gate:** #7 continuity and #15 effect recovery must precede
   unattended or reconnectable work.
4. **Scale gate:** #11 delegation and #25 autonomy require the same permission,
   verification, trace, and recovery guarantees as the single-agent path.
5. **Improvement gate:** #20 traces and #22 evals must exist before adaptive
   context, recovery nudges, or performance work can claim a quality gain.

## Maintaining this map

- Cite active code and an acceptance check when raising a score.
- Re-score after each `PLAN.md` phase and whenever the default execution path
  changes.
- Record path-specific differences instead of crediting one implementation to
  every interface.
- Lower scores when guarantees drift; a feature that exists but is no longer
  wired, secure, or tested is not mature.
