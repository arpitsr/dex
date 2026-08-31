# Harness Canvas

The complete map of what a best-in-class coding-agent harness must own. This
is the product view: every aspect of agent quality, how it is covered today,
and what world-class looks like — so each can be built up deliberately and
tracked over time. Execution order lives in `PLAN.md`; this file is the
board, not the backlog.

## Maturity scale

| Level | Meaning |
|-------|---------|
| **0** | Absent |
| **1** | Basic — works, model carries the quality |
| **2** | Solid — harness adds real intelligence or guarantees |
| **3** | World-class — differentiating; competitors copy it |

## Board

| # | Area | Pillar | Now |
|---|------|--------|-----|
| 1 | Turn loop & orchestration | Cognition | **2** |
| 2 | Goal & task state | Cognition | **0** |
| 3 | Context engineering | Cognition | **1** |
| 4 | Repository understanding | Cognition | **1** |
| 5 | Failure recovery & re-planning | Cognition | **1** |
| 6 | Memory (within & across sessions) | Cognition | **1** |
| 7 | Tool design & reliability | Action | **2** |
| 8 | Verification | Action | **0** |
| 9 | Parallelism & delegation | Action | **1** |
| 10 | Safety, permissions & audit | Trust | **2** |
| 11 | Persistence & crash recovery | Trust | **2** |
| 12 | Terminal UX | Interface | **2** |
| 13 | Non-interactive & CI use | Interface | **2** |
| 14 | Extensibility (skills/hooks/MCP) | Interface | **1** |
| 15 | Model independence | Platform | **2** |
| 16 | Observability & cost | Platform | **1** |
| 17 | Performance & token efficiency | Platform | **2** |
| 18 | Quality measurement (evals) | Platform | **1** |
| 19 | Long-running autonomy | Platform | **0** |
| 20 | Remote & collaboration | Platform | **2** |

---

## Pillar A — Cognition

### 1. Turn loop & orchestration — **2**
The engine: budgets, cancellation, streaming, tool dispatch.

- **Have:** iteration/time/token budgets, wrap-up nudge, cancellation at
  every layer (HTTP, tool process groups), steering mid-turn, parallel
  read-only dispatch with conflict serialization, per-message persistence.
- **World-class:** the loop is boring — every failure mode (provider hang,
  tool panic, timeout storm) ends in a clean, resumable state; batching and
  pacing adapt to model behavior.
- **To close:** mostly polish. Adaptive pacing (slow down on repeated
  failures) arrives free with #5.

### 2. Goal & task state — **0**
The anchor against drift: what are we doing and why.

- **Have:** nothing. The objective exists only as conversation history.
- **World-class:** goal + plan persist across turns, compaction, crashes,
  and resumes; the model always answers "current objective"; user can
  inspect/correct it any time; status visible at a glance.
- **To close:** `PLAN.md` Phase 1. Biggest single quality lever in the repo.

### 3. Context engineering — **1**
What the model sees, when, and at what token price.

- **Have:** summarization compaction (tool-pair safe), usage-driven
  triggering, AGENTS.md injection, skills, `chain` tool that keeps
  intermediate output out of context, opt-in tool cache.
- **World-class:** dynamic working set over a static transcript — relevant
  files, recent changes, git state, failures, and decisions selected per
  turn; compaction preserves orientation facts; token spend proportional to
  task complexity, not session age.
- **To close:** `PLAN.md` Phases 0 and 3 (state restore, turn-start
  snapshot, compaction preserves goal). Later: retrieval beyond grep,
  recently-changed-file awareness.

### 4. Repository understanding — **1**
Knowing the codebase shape without being told.

- **Have:** project instruction files (AGENTS.md/CLAUDE.md), grep/find/git
  tools, workflow discipline in the system prompt.
- **World-class:** harness-level map of the repo — language/toolchain
  detection, test/build commands, module layout, conventions — built once,
  refreshed on change, injected instead of re-explored every session.
- **To close:** start with toolchain + verify-command detection (feeds #8).
  Repo map is Tier 3; `chain` covers most retrieval until then.

### 5. Failure recovery & re-planning — **1**
What happens when the model is wrong, repeatedly.

- **Have:** identical-call blocking, wrap-up nudge, explicit compaction
  failure, HTTP retry/backoff, useful budget-exhaustion message.
- **World-class:** failure patterns are detected (same file, unchanged test
  failure, endless search) and trigger staged strategy escalation; a
  re-plan is proposed to the user, not improvised silently.
- **To close:** `PLAN.md` Phase 4 (failure ledger + escalation nudges).

### 6. Memory (within & across sessions) — **1**
What survives beyond the current turn.

- **Have:** append-only JSONL sessions, resume, durable `/clear`, session
  naming. (Slash state persists but is never restored — `PLAN.md` Phase 0.)
- **World-class:** decisions and discovered facts survive compaction;
  optional cross-session project memory ("we use pnpm, tests via just")
  with user-visible, user-editable storage; nothing remembered silently.
- **To close:** Phase 0 first. Cross-session memory is a deliberate product
  decision — not started, not urgent.

---

## Pillar B — Action

### 7. Tool design & reliability — **2**
The model's hands. Abstraction level and edge-case behavior.

- **Have:** paginated multi-file `read`, ripgrep `grep` with modes,
  bounded `find`, exact-match `edit` with fuzzy fallback, `bash` with
  timeout/output-cap/process-group kill, `git`, `chain` pipelines,
  structured tool errors, malformed-call handling, metadata (read-only/
  mutating) driving scheduling.
- **World-class:** every tool validates at the trust boundary, fails with
  messages that tell the model what to do instead, and never truncates
  silently. Symbol-level navigation and diagnostics are enhancements, not
  the bar.
- **To close:** incremental. Add tools only when a concrete task class
  fails without them.

### 8. Verification — **0**
The difference between "implemented" and "works".

- **Have:** a system-prompt request that the model verify itself. Nothing
  harness-side.
- **World-class:** every mutation triggers relevant verification
  (build/test/lint), results become first-class observations, verification
  status is visible in the UX, and "done" means verified.
- **To close:** `PLAN.md` Phase 2 (explicit `verify_command` first, toolchain
  detection later).

### 9. Parallelism & delegation — **1**
Doing independent work at once without corruption.

- **Have:** parallel read-only tool batches, serialized mutations with
  path-conflict detection, global mutation lock.
- **World-class:** controlled subagents for separable work (explore,
  mass-refactor) with result synthesis in the main loop; safe because the
  single-agent loop underneath is already excellent.
- **To close:** deliberately deferred (see `PLAN.md`). Correct call — a bad
  multi-agent system is worse than none.

---

## Pillar C — Trust

### 10. Safety, permissions & audit — **2**
The harness is running arbitrary code against arbitrary repos.

- **Have:** workspace confinement, four permission modes (read-only /
  ask-writes / ask-shell / trusted), per-session approval memory, remote
  approval flow, audit logging of commands, mutations, approvals, failures.
- **World-class:** the same, plus diffs shown *before* approval, and
  policy-as-config for teams (allowlists per path/tool).
- **To close:** small. Pre-approval diff preview is the one visible gap.

### 11. Persistence & crash recovery — **2**
A crash must never lose completed work or corrupt state.

- **Have:** append-only JSONL, incremental event writes, crash loses at most
  the in-flight operation, durable `/clear`, session versioning and
  validation, malformed-line tolerance, resume by index/path.
- **World-class:** this. Any further work belongs to #6 (memory), not
  durability.
- **To close:** maintenance only.

---

## Pillar D — Interface

### 12. Terminal UX — **2**
Agent cognition made inspectable without noise.

- **Have:** streaming markdown + syntax highlight, per-tool summaries with
  previews and durations, usage in status bar, approval cards, spinner,
  steering/follow-up queue, mouse/scroll, clean terminal restore on
  panic/cancel.
- **World-class:** user sees intent, progress, verification status —
  never raw chain-of-thought. Goal/plan line, verify ✓/✗, cost per turn.
- **To close:** rides along with `PLAN.md` Phases 1–3 (plan row, verify
  line, snapshot).

### 13. Non-interactive & CI use — **2**
Same agent, scriptable.

- **Have:** one-shot prompt mode, raw JSON tool mode, trusted
  non-interactive approval policy, stdin-terminal guard on approvals,
  exit codes from shell errors.
- **World-class:** stable machine-readable output contract, pipeline
  composition (`oye "fix" | verify`), headless daemon already exists.
- **To close:** define the output contract once, document it.

### 14. Extensibility — **1**
Letting users teach the harness without forking it.

- **Have:** skills (SKILL.md discovery, dedup, on-demand load, persisted in
  session), deterministic discovery.
- **World-class:** hooks (pre/post tool, post turn) so users add policy and
  automation; MCP or a custom-tool seam so external tools join the runtime;
  skills stay the low-effort path.
- **To close:** not in `PLAN.md` yet. Hooks first (they also serve #10 and
  #8); MCP when a concrete integration demands it.

---

## Pillar E — Platform

### 15. Model independence — **2**
The model is replaceable; the harness carries the intelligence.

- **Have:** `ModelClient` trait, Chat Completions + Responses wire
  protocols, streaming parsers with fixtures for both, provider auth
  (OpenCode, Codex/ChatGPT), retry/backoff, provider debug log,
  thinking-effort config.
- **World-class:** capabilities and limits come from data, not guesses;
  per-model tool-format and reasoning quirks normalized in one place;
  switching models never changes harness behavior.
- **To close:** `PLAN.md` Phase 5 (static capability table feeding
  compaction). Normalization grows on demand.

### 16. Observability & cost — **1**
Debugging the agent and knowing what it spends.

- **Have:** per-call usage in status bar, provider log for API issues,
  audit log for actions.
- **World-class:** one structured trace per turn (calls, tools, tokens,
  cost, timing) that answers "why did it do that" and "what did this task
  cost" without config.
- **To close:** not in `PLAN.md` yet. Cheap version: extend the existing
  provider log into a per-turn JSON trace file.

### 17. Performance & token efficiency — **2**
Speed and spend per completed task.

- **Have:** `chain` stitching (intermediate output free), batching
  discipline in prompt, output caps, parallel reads, cache-by-fingerprint
  (opt-in), usage-driven compaction.
- **World-class:** token cost per task trends down over time; caching safe
  by default; latency dominated by model, not harness (already true —
  blocking reqwest is fine at this scale).
- **To close:** measure first (needs #16), then optimize what the trace
  shows.

### 18. Quality measurement (evals) — **1**
Knowing changes make the agent better, not just different.

- **Have:** strong unit/integration suite for *infrastructure* (parsers,
  tools, loop with mock model, permissions, recovery) — zero regressions in
  mechanics. No measurement of *agent behavior*.
- **World-class:** a small fixed task suite (seeded repos, scripted mock
  models for determinism + a handful of live-model smoke tasks) run on every
  change to loop/prompt/tool behavior, with pass rate and token cost
  tracked.
- **To close:** not in `PLAN.md` yet. Start tiny: 5 tasks, 1 script. Without
  this, prompt and nudge changes (#2, #5) are unreviewable.

### 19. Long-running autonomy — **0**
Hours-scale work without babysitting.

- **Have:** nothing turn-scoped beyond budgets; remote daemon is the seed
  (an agent that outlives its terminal).
- **World-class:** durable task queue, progress resumable across restarts,
  periodic verification, user check-ins instead of silent drift — all
  built on #2, #5, #8 being solid first.
- **To close:** intentionally last. Every prerequisite is on this board.

### 20. Remote & collaboration — **2**
The agent is not chained to one terminal.

- **Have:** daemon/client split over HTTP+SSE, `oye connect` to attach a
  TUI anywhere, server-side approval parking, remote cancellation, headless
  `serve`.
- **World-class:** this, plus multiple clients on one daemon and session
  sharing. Rare among harnesses; protect this differentiator while building
  the rest.
- **To close:** multi-client only if a real user appears.

---

## How to use this board

- **Read the table** for health: anything at 0 or 1 under Cognition/Action
  is where agent quality is actually lost today.
- **`PLAN.md`** owns the what-next; this file owns the what-exists and
  what-world-class-means.
- **When an area reaches 3**, write down the bar it met — that becomes the
  regression standard (feeds #18).
- Re-score after each `PLAN.md` phase lands. Levels drop on drift, too:
  infrastructure rot (#7, #10, #11) is how harnesses die quietly.
