# Harness Capability Map — Active Runtime

A coding-agent harness is the runtime around the model that turns user intent into controlled, inspectable workspace changes. This file maps what the **supported end-to-end path** actually enforces today. `PLAN.md` sequences future work; this file must not be forgotten or inflated.

**Active runtime:** `dex` → `daemon` (`src/daemon/mod.rs` + `src/daemon/server.rs` axum/Tokio, `DaemonState` with `sessions`/`pending_approvals`/`active_turns`/`cancel_tokens`) → `HTTP/SSE` (`src/protocol/mod.rs` `StreamEvent`, `src/client/http.rs` `DaemonClient` blocking reqwest + SSE) → `TUI` (`src/ui/*` ratatui). Code not wired into that path, or available only in local one-shot (`dex "prompt"`, `dex --tool`, `src/cli/config`), is noted but does not earn score.

## Assessment rules

- **Score only the supported path.** Only `dex→daemon→HTTP/SSE TUI` counts for a full score. Local one-shot, `chain` shell stitching, and unused modules are called out separately.
- **Evidence beats intent.** Prompts, comments, README claims are not guarantees. A capability counts when the active runtime enforces it — cite file + symbol + line.
- **Trust is the weakest exposed path.** A safe `read` does not make an unrestricted `bash` safe; a durable JSONL file does not make daemon resume work; remote transport alone is not collaboration. The least-constrained tool/route defines the trust score.
- **No aggregate score.** A `0` in authorization (#12), containment (#13), verification (#10), or recovery (#7/#15) blocks release regardless of strengths elsewhere.
- **Map maintenance:** raise/lower a score only with a code citation **and** a runnable acceptance check; re-score after each `PLAN.md` phase; record path-specific differences instead of crediting one impl to every interface; lower when guarantees drift (feature exists but no longer wired, secure, or tested).

## Maturity scale

| Level | Meaning |
|-------|---------|
| **0** | Absent, unsafe for the stated use, or not wired into the supported path |
| **1** | Partial / happy-path; relies materially on model, user, or manual recovery |
| **2** | End-to-end harness behavior on the supported path with explicit failure handling and tests |
| **3** | Measured, resilient, policy-ready, protected by behavioral regressions; must name a runnable acceptance check, eval, or SLO |

## Principles (from PLAN.md — still)

- **Injection over state machine.** New behavior = `system` message at existing seam (`src/agent/loop.rs:28 WRAP_UP_THRESHOLD` template, `plan_injection`/`stuck_nudge`).
- **State in daemon, persisted JSONL.** `Session::set_state` / `load_session_state` (`src/session.rs:268` / `387`) is the store; restore is explicit, not ambient.
- **Ceiling, not flag.** Daemon policy beats client request. Fail closed (`src/daemon/server.rs:427 permissiveness` check → `403`).
- **One phase ships.** Independently useful, tested, then next.

## Board

`P0–P6`, `P8–P10` + task budget shipped; `P7`, `P11+` planned. `—` = gap not yet planned. `✓` = shipped and wired on active path.

| # | Area | Pillar | Now | Planned | Gate |
|---|------|--------|-----|---------|------|
| 1 | Objective, acceptance & progress | Direction | **2** | budget ✓, evidence-link pending | Correctness |
| 2 | Turn orchestration & stop semantics | Direction | **2** | P10 reattach/replay ✓ | Recovery |
| 3 | Failure recovery & re-planning | Direction | **2** | P4 ✓ | — |
| 4 | Instruction hierarchy & behavior policy | Knowledge | **1** | — | — |
| 5 | Context lifecycle | Knowledge | **2** | P3 ✓ | Improvement |
| 6 | Repository & environment intelligence | Knowledge | **1** | P2 verify ✓, repo map pending | — |
| 7 | Memory & session continuity | Knowledge | **2** | P8 rebuild ✓, P0 partial ✓ | Recovery |
| 8 | Tool protocol & reliability | Action | **2** | — | — |
| 9 | Change control & artifact integrity | Action | **2** | P8 ✓ (hash-ledger + undo) | Correctness |
| 10 | Verification & completion evidence | Action | **2** | P2 ✓, P9 ✓, gate open | Correctness |
| 11 | Parallelism & delegation | Action | **1** | Deferred | Scale |
| 12 | Authorization, approvals & audit | Trust | **2** | P6 ✓ (ceiling+scoped audit) | Remote |
| 13 | Isolation & resource containment | Trust | **1** | — (cheap confinement before bwrap) | Remote |
| 14 | Secrets, privacy & remote access security | Trust | **0** | P7 token+redaction | Remote **blocker** |
| 15 | Durability & effect recovery | Trust | **2** | P8 ✓ (journal+rebuild) | Recovery |
| 16 | Interactive UX & human control | Interface | **2** | P1–P3 partial ✓ | — |
| 17 | Automation & protocol contracts | Interface | **2** | P10 ✓ (versioned+replay) | Recovery |
| 18 | Extensibility & integrations | Interface | **1** | — | — |
| 19 | Model/provider portability | Platform | **2** | P5 ✓ | — |
| 20 | Observability, usage & cost | Platform | **2** | P9 ✓ (trace/cost) | **Improvement gate** |
| 21 | Performance & token economy | Platform | **2** | P3/P5 partial ✓, budget ✓ | Improvement |
| 22 | Quality evaluation | Platform | **1** | P9 trace ✓, evals pending | **Improvement gate** |
| 23 | Operations, configuration & compatibility | Operations | **1** | — | — |
| 24 | Remote use & collaboration | Operations | **1** | P10 reattach/replay ✓, identity pending | Remote/Recovery |
| 25 | Long-running autonomy | Operations | **0** | Deferred (needs #7/#15 — both now 2) | Scale |

Release gates: **Remote** (#12+#13+#14) — `0` at #14 still blocks non-local daemon. **Correctness** (#1+#9+#10) — **closed** (`2/2/2`): dispositions exist, but `acceptance` criteria are still a user checklist — evidence-linking them to recorded `verify` results is the next step. **Recovery** (#7+#15) — **closed** (`2/2`): restart rebuilds the registry, interrupted turns become durable `turn_failed`, reattach replays. **Improvement** (#20+#22) — `#20` open, `#22` still `1`: no eval suite, so adaptive changes still cannot claim quality gain.

---

## Pillar A — Direction

### 1. Objective, acceptance & progress — **2**

**Owns:** durable task contract: goal, constraints, acceptance criteria, plan, current step, completion state, user correction.

- **Evidence (supported path):** `core::types::Plan{goal, constraints, steps, acceptance, budget}` (`src/core/types.rs:18`) with `#[serde(default)]` on the level-2+ fields (pre-2 JSONL still loads) persisted via `Session::set_state("plan", json)` from `ui/slash` `/goal` `/plan add|done|clear` `/constraint add|clear` `/accept add|done|clear` `/budget [seconds] [iterations] [cost]` (`src/ui/slash.rs:27`); remote sync via `protocol::ChatRequest.plan` → `daemon/server.rs` `set_state` with **explicit validation** (invalid plan JSON → `Err` → `TurnFailed`; persistence `io::Error` → turn failure). Budget is enforced by the loop: `plan_budget` tightens `TurnLimits.elapsed_seconds`, caps `iteration_cap`, and accumulates `max_cost_usd` spend against prompt tokens (`src/agent/loop.rs` turn start), injected into the `name:plan` summary (`Budget: 120s, 5 tool iterations, $0.50`). `SinkLine::Plan`→`StreamEvent::Plan` carries all five fields incl. `budget` (`src/protocol/mod.rs`), remote TUI reconstructs the full contract (`src/ui/remote.rs`). Acceptance: `cargo test plan_contract_summary_includes_completion_state plan_round_trip_keeps_contract_and_loads_legacy_json plan_is_injected_before_first_model_call plan_budget_caps_iterations_earlier_than_config` (89 total, clippy clean).
- **Path difference:** `/resume` with args, `/name`, `/provider` restore works local/one-shot; remote reattach (`--reattach`) now restores session + replay, but model/provider restore stays on the daemon host (not forwarded-back on reattach).
- **Gap:** acceptance criteria are a user-maintained checklist, not evidence-linked — no harness stop condition tying `acceptance` to recorded tool/verify results (P9 now records `disposition`, but nothing links a criterion to a disposition). `Level 3` requires state survives compaction/restart (✓), reconnect (✓ via reattach), user can edit (✓); every acceptance criterion links to a result or explicit waiver — the remaining step is linking `is_complete()` to the `verify`/`changes` ledger.

### 2. Turn orchestration & stop semantics — **2**

**Owns:** model/tool iteration, event ordering, budgets, deadlines, cancellation, backpressure, clean terminal outcomes.

- **Evidence:** `src/agent/loop.rs:361 process_turn` — iteration limit (`LlmConfig.max_tool_iterations`, now tighten-able per-task via `Plan.budget.max_tool_iterations`), prompt-token + elapsed-time limits (`TurnLimits`/`deadline`/`within_budget`, budget-aware), streaming `ModelClient::complete`, wrap-up nudge at `WRAP_UP_THRESHOLD=5`, poll-based `CancellationSource`. Daemon enforces one turn/session (`active_turns` → `409 CONFLICT`), per-session `CancellationToken`, `TurnGuard` drop cleans `active_turns`/`cancel_tokens`/`pending_approvals`, terminal `StreamEvent::TurnComplete/TurnFailed` now numbered + journaled as `StreamEnvelope{seq}` (P10) with durable `turn_start/turn_complete/turn_failed` markers (P8).
- **Gap:** deadline checked between model rounds only; approval waits (`approval_rx.recv`) and retry sleeps have no shared deadline. SSE disconnect now reattachable (replay via `/events?since=`), but no cancel-or-continue contract on disconnect. Mixed batch wholly serialized when any call mutates (`TOOL_MUTATION_LOCK`).
- **L3 bar:** one deadline + cancellation contract covers provider calls, retries, approvals, tools, delivery; disconnects/panics end in deterministic terminal state; chaos tests prove no wedged turn.
- **L3 bar:** one deadline + cancellation contract covers provider calls, retries, approvals, tools, delivery; disconnects/panics end in deterministic terminal state; chaos tests prove no wedged turn.

### 3. Failure recovery & re-planning — **2**

**Owns:** recognizing failing approach, changing strategy, escalating instead of looping.

- **Evidence:** provider retry/backoff (`src/llm/client.rs`), structured tool failures, malformed-call handling, repeated-successful-call block (`src/agent/loop.rs:642` `repeated_count>=3`), wrap-up nudge + `P4` ledger: `last_failed` (identical failed `cache_key` ≥3), `edit_paths` (same path 3×), `search_streak` (`grep`/`find` ≥4 without `read`), `last_verify_hash`, `escalation_count` capped 3 via `stuck_nudge` 1:direct fix 2:arch 3:question assumption 4:re-plan → abort (`src/agent/loop.rs:79`).
- **Gap:** ledger is per-turn in-memory, not durable `failure ledger`; no staged `re-plan` object; no stuck-agent eval proving `1→2→3→re-plan` terminates with preserved partial work.
- **L3 bar:** deterministic signatures trigger bounded escalation; recovery visible; scripted stuck-agent evals terminate with useful re-plan and preserved partial work.

---

## Pillar B — Knowledge

### 4. Instruction hierarchy & behavior policy — **1**

**Owns:** ordering/bounding/attributing system policy, project instructions, skills, user requests, retrieved content.

- **Evidence:** `src/llm/prompt.rs:system_prompt` builds static system prompt, walks upward for first `AGENTS.md`/`CLAUDE.md`, advertises discovered skill names/descriptions (`src/skills/*`).
- **Gap:** precedence/provenance implicit; project instruction size unbounded; no effective-prompt inspection or instruction/data boundary. Active remote TUI cannot load skill body with `/skill:<name>` although prompt advertises it (skill load is `POST /api/sessions/{id}/skill` on daemon, not wired to slash on remote path). No policy tests for conflicting/malicious repo instructions.
- **L3 bar:** deterministic precedence, source labels, token budgets, policy tests; users can inspect which instructions affected a turn.

### 5. Context lifecycle — **2**

**Owns:** selecting, ordering, compressing, invalidating, restoring what the model sees.

- **Evidence:** full session replay (`src/session.rs:337 load_messages_from_session` respects `clear` markers), usage/estimate-driven summarization (`src/agent/compaction.rs:232 compact_history` with `KEEP_RECENT_MESSAGES=12`, `MIN_MESSAGES_TO_SUMMARIZE=8`, `estimate_tokens` + `effective_tokens`), tool-call/result pair protection (`find_cutoff`), output caps (`src/skills/tools/mod.rs:251 MAX_LINE_CHARS=2000/clamp_lines`, `CONFIGURED_OUTPUT_LIMIT` 1 MiB via `ghjk` precedent), bounded `read`/`grep`/`find`, plus `P3` `turn_start_context` once/turn `system name:context` = `Plan` summary + `git status --short` + `git diff --stat` via `tool_git`, capped 10 lines (`src/agent/loop.rs:41 capture_git_context`), ephemeral injection (`plan_text`/`git_ctx`/`nudge_text` in `effective_messages` but never appended to `messages` — `src/agent/loop.rs:521`), `summarize_old_messages` preserves `plan`/`verify` verbatim + deterministic fallback (`src/agent/compaction.rs:82`/`129`). Pre-call compaction loop with session re-persist (`clear_messages` + re-append) and hard-limit check (`src/agent/loop.rs:467`/`505`). Tests: `plan_is_injected`, `plan_context_is_ephemeral`, `compaction_runs_before_model_call`.
- **Gap:** no provenance-bearing `context manifest`, no stale-fact invalidation, no eval tracking answer quality/tokens across growth.
- **L3 bar:** manifest rebuilt per turn, stale facts invalidated, required facts survive compaction, evals track quality/tokens.

### 6. Repository & environment intelligence — **1**

**Owns:** project map, toolchains, modules, conventions, dependency shape, build/test commands, change-aware refresh.

- **Evidence:** project instruction discovery; `read`/`grep`/`find`/`git` (read-only) + branch/dirty in `DaemonInfo` (`src/daemon/server.rs:50 git_context`) shown in UI footer; `P2` explicit `verify_command` from `DEX_VERIFY`/`config.sample.json` (no auto-detection) run via `bash` with same caps/cancel (`ToolState::verify_dirty` in `src/agent/loop.rs`); `P3` turn-start `git status/diff --stat` snapshot.
- **Gap:** no cached repo map, toolchain/package-manager detection, auto verification-command discovery, symbol/diagnostic index, environment readiness check. Workspace is daemon CWD. Model still discovers rest manually via `grep`/`chain`.
- **L3 bar:** small inspectable map built once, invalidated by actual changes, supplies correct build/test commands and navigation with less re-exploration.

### 7. Memory & session continuity — **2**

**Owns:** what survives turns, compaction, reconnects, new processes.

- **Evidence:** append-only versioned JSONL (`src/session.rs:11 SESSION_VERSION=1`), incremental `append_message`/`set_state`/`clear_messages`, malformed-line tolerance, `clear` marker, `File::sync_data` on every append (P8 — a reported success is on disk). `P0` wired `load_session_state` + `load_plan`/`save_plan`, `apply_session_state` restores `model`+`provider`+`plan` on local `/resume`. P8/P10 close the persistence gaps on the scored path: `daemon::rebuild` (`src/daemon/mod.rs`) restores the session registry from persisted JSONL at startup (`rebuild_marks_interrupted_turns_failed_and_registers_sessions`), seeds the per-session SSE cursor from the journal (`event_seq_is_seeded_from_disk_after_restart`), and flips interrupted turns to durable `turn_failed` + `last_error`; `POST /api/sessions/{id}/reattach` + `GET /events?since=` let a client replay a persisted session (`dex connect --reattach <id>`, `src/ui/remote.rs`).
- **Path difference (scored):** registry now rebuilt from disk on daemon start (`state.rebuild()` in `run_daemon`); `GET /api/sessions` lists persisted sessions with `turn_state`. Remote reattach restores session + transcript replay; restoring `model`/`provider` overrides on the remote path is still pending (they live on the daemon host).
- **Gap:** no explicit project memory (remembered facts are user-visible/editable); `/resume` on the remote path still goes through `--reattach` rather than the local selector UX.
- **L3 bar (P8):** all active paths restore same task/model/skill/session after restart (session ✓, model/provider pending); remembered facts user-visible, editable, scoped, attributable, removable.

---

## Pillar C — Action

### 8. Tool protocol & reliability — **2**

**Owns:** schemas, validation, result/error contracts, timeouts, output shaping, cancellation, side-effect metadata.

- **Evidence:** bounded `read`/`grep`/`find`/`git`/`chain`; `bash` with timeout/output capture/process-group kill; exact/fuzzy `edit`; `write`; structured internal errors; malformed-call handling; `metadata.read_only` / `is_mutating` (`src/skills/tools/mod.rs`); `MAX_LINE_CHARS`/`clamp_lines`; `CancellationSource` checked in `execute` and `call_client_cancellable` (`src/agent/loop.rs:312`). Independent read-only calls run concurrently; mutating batches serialized under `TOOL_MUTATION_LOCK` (`src/core/console.rs:120`).
- **Gap:** truncation/fan-out limits not consistently machine-readable; search assumes host Unix tools (`grep`/`find`); metadata not yet a proved side-effect contract. Symbol navigation/diagnostics only on demonstrated task failures (per PLAN deferred).
- **L3 bar:** contract/property tests cover every arg, truncation, cancellation, side-effect claim; failures prescribe valid next action; no silent incompleteness.

### 9. Change control & artifact integrity — **2**

**Owns:** preserving user work while applying, reviewing, grouping, reverting, handing off changes.

- **Evidence:** unique-match `edit` (+ `expected_hash` stale guard — `tools::check_expected_hash` → `ToolError::StaleFile`, a 409-class refusal: `write_edit_require_expected_hash_and_reject_stale`), workspace path checks for file tools, serialized mutation batches, per-change ledger in `session_state "changes"` — `before/after` content (capped 64 KiB) + hashes via `session::record_change`/`make_change_record`, diff preview surfaced as `SinkLine::System "[change preview]"` before approval when `permission != trusted` (`loop.rs execute_tool_call` → `tools::change_preview`), `/undo` (local `slash.rs` + `POST /api/sessions/{id}/undo`) restores the last record and refuses if the file's `after_hash` moved (409) — `change_ledger_records_then_undo_restores`, `undo_refuses_when_file_moved_on`.
- **Gap to 3:** no baseline separating pre-existing changes; shell mutations (`bash`) bypass file-level accounting (only its audit line exists); no enforced final diff review; undo refuses files > 64 KiB.
- **L3 bar:** every mutation belongs to reviewable change set with preconditions and before/after evidence; concurrent user edits never overwritten silently; cancellation can reconcile/roll back partial work.

### 10. Verification & completion evidence — **2**

**Owns:** deciding relevant checks, running them after changes, feeding failures back, defining “done.”

- **Evidence (P2+P9):** `ToolState::verify_dirty` set on mutating batch, runs `verify_command` via `tools::execute_outcome("bash")` same caps/cancel. `verify_command` comes from config or is auto-detected at the daemon/one-shot boundary (`llm::config::detect_verify_command` — Cargo.toml/go.mod/package.json), so no manual `DEX_VERIFY` needed. Outcome is persisted to `session_state "verify"` with `disposition: pass|fail` (+ tail hash on fail) — see `effect_journal_verify_disposition_and_trace_are_written`; `/waive <reason>` (local + `POST /api/sessions/{id}/waive`, empty reason → 400) records `disposition: waived` with a `name:waive` message the model sees. Fail → `user name:verify` tail fed to the next model call; verify runs inside the iteration budget.
- **Gap to 3:** acceptance criteria are not linked to recorded dispositions (the evidence-linking stop condition is the next step); no enforced final diff review; `waived` is user-initiated, not budgeted.
- **L3 bar:** every change has verification disposition; relevant build/test/lint/diagnostic checks run against final state; failures return to loop; final response links claims to recorded results.

### 11. Parallelism & delegation — **1**

**Owns:** dependency-aware concurrent work, conflict isolation, subtask budgets, synthesis.

- **Evidence:** independent read-only calls in one model response run in parallel (`src/agent/loop.rs:596` thread spawn); separate daemon sessions may run concurrently (different `session_id` → different `active_turns` entry).
- **Gap:** no DAG or normalized path locking; any mutation serializes its whole batch; no subagents/worktrees/delegated budgets/result contracts/merge step. Deferred until #9/#12/#15/#20 guarantees exist (PLAN).
- **L3 bar:** separable subtasks show lower wall time without reducing pass rate; each worker isolated, budgeted, cancellable, merged through same verification/change-control gates.

---

## Pillar D — Trust

### 12. Authorization, approvals & audit — **2** (P6 ✓)

**Owns:** who may request effect, what may be done, approval scope/expiry, policy enforcement, attributable record.

- **Evidence (active path):** four modes `PermissionMode::{ReadOnly,AskWrites,AskShell,Trusted}` (`src/core/types.rs:218`) with `permissiveness()` (`src/core/types.rs:243`). Daemon ceiling: `daemon_perm` from file/env (`src/daemon/server.rs:427` `permission_from_env_or_file`) is max; `ChatRequest.permission` may only be equal or stricter — escalation returns `Err("permission escalation denied")` → `StreamEvent::TurnFailed` (fail-closed, `403` semantics). Console approval key scoped: `write`/`edit`→`path` hash, `bash`→`command` hash, else input hash (`src/core/console.rs:202 approval_key`); per-turn expiry via `Console` drop (no wall-clock 10m yet); `session_approved`/`record_session_approval` (`src/core/console.rs:230`). Daemon approval parking `DaemonState.pending_approvals` by `request_id` (`src/daemon/mod.rs:27`, `src/daemon/server.rs:534` parked, `src/daemon/server.rs:591 approve`), cross-session restore on mismatch. Audit best-effort `audit.jsonl` with `actor`/`request_id`/`input_hash`/`decision`/`timestamp` — local `audit_approval` (`src/agent/loop.rs:99`) and remote `daemon/server.rs:610` `actor:"remote"`; `0600` intended (P6 writes without explicit mode, P7 adds). Tested: `cargo test --all-targets` (63) + manual `DEX_PERMISSION=ask-writes` daemon, client `trusted` → denied.
- **Gap to 3:** approvals lack `diff` hash, no 10m wall-clock expiry, actor is `local`/`remote` not token principal, audit best-effort not tamper-evident, no policy rules.
- **L3 bar:** daemon owns non-bypassable ceiling; clients may only request equal/stricter unless authorized; approvals scoped to paths/commands/diffs with expiry; every decision has actor + tamper-evident record.

### 13. Isolation & resource containment — **1**

**Owns:** blast radius across filesystem, process, network, environment, CPU/memory/disk/time.

- **Evidence:** direct file tools canonicalize and reject symlink escapes (`src/skills/tools/mod.rs` `OutsideWorkspace`); shell output/time bounded and descendants killed as process group; `chain` rejects mutating tools.
- **Gap:** `bash` runs as daemon user with inherited env and unrestricted filesystem/process/network. Workspace start is not confinement. No CPU/memory/disk/PID/egress limits. `bwrap`/nsjail per-turn mount+net+quota is correct but is new binary/privilege — PLAN does cheap confinement (P6–P8) first.
- **L3 bar:** untrusted turns in tested sandbox with explicit mounts, env allowlists, net policy, quotas, reliable teardown; trusted escapes conspicuous + auditable.

### 14. Secrets, privacy & remote access security — **0** — **Remote gate blocker**

**Owns:** daemon auth, transport security, credential boundaries, redaction, retention, provider egress, session ownership.

- **Evidence:** provider credentials from env/config; `Responses` `store:false`; tool caching opt-in (`DEX_TOOL_CACHE=1` only, `src/agent/state.rs:77`); binding to `0.0.0.0` prints warning (`src/cli/*`).
- **Gap — blocker for remote use:** HTTP daemon has no authentication or TLS (`src/daemon/server.rs:31 router` — `/health` + `/api/*` unauthenticated). Unauthenticated chat can override `base_url` and `permission` (up to ceiling) — `store:false` alone does not prevent credential exposure via custom endpoint; trusted shell = RCE. Sessions, tool args, `write` contents, provider error bodies stored plaintext without redaction, retention policy, or `0600` enforcement. `P7` planned: `DEX_TOKEN` file (`~/.config/dex/token` `0600`) or env, `Authorization: Bearer` on every `/api/*` (health exempt), server policy forbids `base_url`/`permission` overrides for remote, redaction/truncation, `DEX_RETENTION_DAYS` 30.
- **L3 bar:** authenticated principals + session ownership; provider endpoints + ceilings are server policy; remote traffic over trusted secure channel; secrets never enter prompts/logs by default; retention/export/delete testable.

### 15. Durability & effect recovery — **2**

**Owns:** reconstructing conversation + side-effect state after crashes, partial writes, disconnects, retries without duplicating effects.

- **Evidence (P8):** versioned append-only JSONL with `File::sync_data` per append; durable `turn_start`/`turn_complete`/`turn_failed` markers (`run_turn_inner` propagates `io::Error` on plan/turn_start/prompt/terminal writes → `turn_failed`, no `let _ =` swallow); per-tool `effect_start{tool_call_id,name,input_hash}` before execution and `effect_result{tool_call_id,ok}` after (`session.rs`, written by the loop — `effect_journal_records_intent_and_outcome`, `effect_journal_verify_disposition_and_trace_are_written`); `daemon::rebuild` restores registry + event cursor from disk and marks interrupted turns durable-`turn_failed` with a `last_error` state (smoke-tested with curl: restart shows `turn_state: failed`). SSE events are journaled with `seq` for reattach replay (P10).
- **Gap to 3:** restart reconciles *some* unknown effects (the interrupted-turn marker + effect journal exist) but has no auto-rollback of partial `bash` effects; idempotency dedup is per-process (60s window) and does not survive a restart; migrations/corruption tests missing.
- **L3 bar:** intent, approval, effect start, result, final hashes form recoverable journal; restart reconciles unknown effects, cannot repeat non-idempotent action silently; migrations/corruption tested.
- **L3 bar:** intent, approval, effect start, result, final hashes form recoverable journal; restart reconciles unknown effects, cannot repeat non-idempotent action silently; migrations/corruption tested.

---

## Pillar E — Interface

### 16. Interactive UX & human control — **2**

**Owns:** showing intent, progress, actions, approvals, outcomes; timely steer/cancel/correct without exposing hidden reasoning.

- **Evidence:** streaming markdown TUI (`src/ui/render.rs`), tool summaries/previews/durations (`SinkLine::ToolOutput` `src/core/types.rs:59` → `StreamEvent::ToolResult` `src/protocol/mod.rs:83`), live `Usage` (`SinkLine::Usage`/`StreamEvent::Usage`), approval overlay (`PendingApproval`/`ApprovalRequired`), `CancellationToken` cancel (`src/daemon/server.rs:670`), scroll/history/autocomplete, branch/dirty footer, `INTERRUPTED` sigaction without `SA_RESTART` + `SpinnerGuard` (`src/core/console.rs:20`/`270`). `Goal:·Plan 3/7` in `plan_status`/`footer_text` + `plan`/`context` system messages + `verify ✓`/`name:verify` feedback. Remote `SinkLine::Plan`→`StreamEvent::Plan` keeps bar in sync.
- **Gap (active path):** mid-turn `steering`/`follow-ups` not wired — `process_turn` `steering_rx=None` in `daemon/server.rs:574` and Enter ignored while busy despite dormant fields/docs. `/name` `/undo` `/waive` `/budget` now work remotely; `/resume` routes to `dex connect --reattach`; provider switching stays daemon-side. Accessibility not proven.
- **L3 bar:** UI truthfully shows task/step, current action, queued input, change set, verification; reconnect preserves control; all help text exercised against active path; keyboard-complete, not color-dependent.

### 17. Automation & protocol contracts — **2**

**Owns:** headless use, stable machine I/O, exit semantics, event schemas, reconnect/replay, CI integration.

- **Evidence (P10):** local one-shot prompting, JSON-lines raw tool mode, `dex --tool` `run <tool>`, daemon HTTP/SSE with `health`/`chat`/`approve`/`cancel`/`config`/`skills`. Versioned: client sends `Accept: application/vnd.dex.v1+json` + `X-Dex-Protocol: 1` on every `/api/*`; server rejects unknown protocol on `/chat` (400). Every stream event is `StreamEnvelope{seq,event}` (`src/protocol/mod.rs`), journaled to `<session>.events.jsonl` (`session.rs append_event`/`load_events`) and replayable via `GET /api/sessions/{id}/events?since=`; `POST /chat` dedups `Idempotency-Key` for 60s per session+request-hash (`DaemonState::idempotent_replay/record` — `idempotency_key_replays_same_turn_and_rejects_different_request`); `GET /api/sessions` lists persisted JSONL; `POST /api/sessions/{id}/reattach` returns the cursor.
- **Gap to 3:** no final-result JSON contract (outcome/changes/checks/usage as one record — trace rows approximate it), no `seq` dedup across restart (idempotency map is in-memory), disconnect cancel-or-continue undefined.
- **L3 bar:** versioned protocol with compat tests, deterministic exit codes, resumable streams, idempotency keys, one structured final record (outcome, changes, checks, usage, unresolved work).

### 18. Extensibility & integrations — **1**

**Owns:** adding instructions, policies, lifecycle automation, tools, external systems without forking.

- **Evidence:** deterministic skill discovery with frontmatter metadata and `skill_dirs` (`src/skills/*`, `src/llm/prompt.rs` advertises `name`/`description`).
- **Gap:** skill bodies cannot be loaded on demand through default daemon/TUI path (`POST /api/sessions/{id}/skill` exists but not wired to `/skill:<name>` slash on remote). No pre/post-tool or post-turn hooks, custom-tool registration, MCP seam, capability declarations, or isolation/versioning.
- **L3 bar:** small versioned extension contract covers lifecycle, permissions, cancellation, errors, testing; external tools cannot bypass auth/observability; plain `SKILL.md` remains zero-code path.

---

## Pillar F — Platform

### 19. Model/provider portability — **2** (P5 ✓)

**Owns:** wire adaptation, capabilities, auth, streaming/tool-call normalization, limits, retries, fallback, model switching.

- **Evidence:** `ModelClient` boundary (`src/llm/client.rs`), Chat Completions/Responses translators (`src/llm/chat_completions.rs`/`responses.rs`/`protocol.rs`), OpenCode/Codex credential paths (`src/llm/auth.rs`), streaming (`src/llm/streaming.rs`), retry/backoff, usage extraction, `reasoning_effort`, plus `P5` static `provider_default_context_window` + honest `discover_capabilities` table (no probing) driving `context_window/2` compaction without manual config (`src/llm/config.rs`→`src/agent/loop.rs:469`). `LlmConfig::from_env` falls back to provider default.
- **Gap:** limits static not probed, quirks manual, coverage OpenAI-shaped, retry ignores `Retry-After`, no fallback policy.
- **L3 bar:** data-backed capabilities + conformance fixtures normalize providers; switching models preserves harness semantics; fallback explicit, bounded, measured.

### 20. Observability, usage & cost — **2** — **Improvement gate (open)**

**Owns:** correlated traces for model calls, tools, approvals, tokens, latency, cost, failures, final outcomes — without chain-of-thought or secrets.

- **Evidence (P9):** live prompt-token `Usage` per LLM call; per-turn redacted `trace.jsonl` (`0600`, `TraceWriter`) with spans `{kind: turn|llm|tool|verify, turn_id, tool_call_id, duration_ms, prompt_tokens, cost_usd, ok, approval_required, effect_hash}` — no tool args, prompts, or secrets (`effect_journal_verify_disposition_and_trace_are_written` asserts redaction); `GET /api/sessions/{id}/trace` returns rows; cost = `prompt_tokens × DEX_COST_PER_1K` (default $2/M, approximate); tool `audit.jsonl` + provider `provider.jsonl` still exist.
- **Gap to 3:** no completion-token accounting, no cache/retry metrics, no log rotation/retention, no queryable per-task outcome view (trace rows are per-turn, correlation across turns by session), cost is a static approximation not provider price tables.
- **L3 bar:** redacted spans share task/turn/call IDs and record timing, usage, price, approvals, effects, checks, outcome; budgets + regressions visible without reconstructing raw logs.
- **L3 bar:** redacted spans share task/turn/call IDs and record timing, usage, price, approvals, effects, checks, outcome; budgets + regressions visible without reconstructing raw logs.

### 21. Performance & token economy — **2**

**Owns:** latency, round trips, prompt/tool-output size, cache correctness, resource use, cost per successful task.

- **Evidence:** batched read guidance, concurrent read-only calls (`src/agent/loop.rs:584`), bounded fan-out, `chain` to reduce round trips, `$DEX_BIN` shell-side distillation (`src/client/*`), output clamps (`MAX_LINE_CHARS=2000` + `CONFIGURED_OUTPUT_LIMIT` 1 MiB), summarization (`src/agent/compaction.rs`), opt-in tool caching (`src/agent/state.rs:77`), plus `P3/P5` honest per-provider `context_window` driving `compact_history` thresholds (`effective_tokens > context_window/2` in `src/agent/loop.rs:469`) without manual `DEX_CONTEXT_WINDOW`.
- **Gap:** no task-level benchmark tying improvements to success/latency/cost. Compaction costs a model call; cache not strong enough to enable by default for all searches. Task budgets (`Plan.budget`) now give the knob for spending, but there is no benchmark measuring it.
- **L3 bar:** fixed evals report p50/p95 latency, calls, tokens, cost per successful task; optimizations preserve quality/correctness; cache invalidation proved before default enablement.

### 22. Quality evaluation — **1** — **Improvement gate**

**Owns:** proving prompt/context/tool/loop/safety/UX changes make real tasks better.

- **Evidence:** CI runs `cargo fmt -- --check`, `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`, `audit-check`; in-crate tests cover tools/rendering/compaction/loop; `network-tests` feature exists.
- **Gap:** no seeded coding-task suite, no behavioral harness across daemon/API/session recovery, no live-model smoke set, no safety/adversary suite, no tracked quality/cost baseline. `network-tests` currently adds no tests; infra tests do not measure agent success. With #20 open but `#22` still at 1, adaptive changes cannot yet be gated on quality.
- **L3 bar:** deterministic scenarios + small live-model set gate relevant changes on task success, safety, latency, cost; failures retain replayable traces; accepted score changes explicit.

---

## Pillar G — Operations

### 23. Operations, configuration & compatibility — **1**

**Owns:** startup/shutdown, readiness, config validation, deployment, quotas, upgrades, protocol/session migration, supportability, platform scope.

- **Evidence:** JSON/env config (`src/llm/config.rs`), `GET /health` (`src/daemon/server.rs:44`), `DaemonClient::wait_until_ready` polling (`src/client/http.rs:38`), localhost-by-default bind, configurable limits, CI.
- **Gap:** no authenticated deployment profile, graceful lifecycle, config schema/version, API compat policy, session migration, log retention, per-user/turn quotas, or restart reconstruction. Unix process-group behavior assumed not declared as platform limit.
- **L3 bar:** validated config + safe defaults fail closed; supervised restart preserves supported sessions; upgrades pass protocol + migration tests; health/capacity/resource limits observable.

### 24. Remote use & collaboration — **1**

**Owns:** operating away from workspace host and, separately, sharing sessions/control/review among people.

- **Evidence:** daemon/client separation over `HTTP+SSE`, remote streaming/approvals/cancel, multiple independent daemon sessions, P10 persisted attach + replay: `--reattach`, `GET /events?since=`, disk-backed session listing.
- **Gap:** still blocked on #14 (`0`) for non-local daemon; user identity, ownership, shared-session concurrency, handoff, review link, multi-client control all missing. Replay/reattach exist; collaboration does not.
- **L3 bar:** authenticated users can discover + reattach to owned/shared sessions, replay missed events, transfer control, review same change + verification record without races.

### 25. Long-running autonomy — **0**

**Owns:** durable queued work lasting hours/restarts, bounded unattended execution, checkpoints, verification cadence, user check-ins.

- **Evidence:** only per-turn budgets (`TurnLimits`) and daemon process; no durable scheduler.
- **Gap:** no queue, leases, checkpoint policy, restart continuation, unattended approval policy, heartbeat, or spend/resource budget. The Recovery gate bottom is now solid (#7+#15 both 2) so this is no longer blocked by journaling — it is deferred by choice: autonomy without an unattended-approval policy. Deferred per PLAN.
- **L3 bar:** restartable tasks make monotonic, verified progress under explicit time/cost/effect budgets; risky boundaries pause for user; stalled tasks stop + report.

---

## Cross-cutting release gates

1. **Remote gate (#12+#13+#14):** all must be solid before non-local daemon is safe. Now `2/1/0` — **blocked by #14**.
2. **Correctness gate (#1+#9+#10):** defines whether “done” means anything. Now `2/1/1` — `P9` verification disposition required.
3. **Recovery gate (#7+#15):** must precede unattended/reconnectable work. Now `1/1` — `P8` journal + rebuild required.
4. **Scale gate (#11+#25):** requires same permission, verification, trace, recovery as single-agent path. Deferred.
5. **Improvement gate (#20+#22):** traces + evals must exist before adaptive context, recovery nudges, or perf work can claim quality gain. Now `1/1` — `P9` trace/cost + eval required.

## Maintaining this map

- Cite active code (file + symbol) and a runnable acceptance check when raising a score.
- Re-score after each `PLAN.md` phase and whenever the default path changes.
- Record path-specific differences; do not credit one implementation to every interface.
- Lower scores when guarantees drift — a feature that exists but is no longer wired, secure, or tested is not mature.

## Validation

Every phase: `cargo fmt -- --check`, `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`. Each lands with its own behavioral test (mock `ModelClient` + temp workspace + `NeverCancel`) before next starts. Re-score this file after phase with code ref + acceptance check. See `PLAN.md` Order recap for gate sequence.
