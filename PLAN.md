# Harness Plan

Gap analysis against a best-in-class coding-agent harness: the loop, safety,
tools, model adapters, and UX are solid; what is missing is harness-side
**intelligence** — goal/plan state, verification, recovery, and dynamic
context. Everything below reuses existing primitives (`Session::set_state`,
the system-nudge injection pattern, `SinkLine` events, the steering channel,
`is_mutating`). No rewrites, no new abstractions.

## Principles

- **Injection over state machine.** Do not rewrite the loop as an FSM. New
  behavior arrives as system-role messages injected at existing points (the
  `WRAP_UP_THRESHOLD` nudge in `agent/loop.rs` is the template).
- **State lives in the daemon, persists via sessions.** Plan/verification
  state is daemon-owned; the session JSONL is the store.
- **One phase ships before the next starts.** Each phase is independently
  useful and testable.

---

## Phase 0 — Wire up existing dead persistence (small, unblocks Phase 1)

`Session::set_state()` is called by `/model` and `/provider`, but
`load_session_state()` (`src/session.rs`) is `#[allow(dead_code)]` — state is
written and never restored.

- [ ] Call `load_session_state` on `/resume` and re-apply model/provider.
- [ ] Remove the `dead_code` allow.
- **Accept:** resume a session that switched models → model is restored.

## Phase 1 — Goal and plan state

The model currently has no answer to "what am I accomplishing and why".
Add a plan object that outlives the transcript and is re-injected every turn.

- [ ] Add `Plan` to daemon agent state:
  ```rust
  struct Plan {
      goal: Option<String>,          // /goal — why
      steps: Vec<(String, bool)>,    // ordered, done flags — what
  }
  ```
- [ ] Persist via `session.set_state("plan", json)` on every change; restore
  via Phase 0 loader.
- [ ] Slash commands in `ui/slash.rs`: `/goal <text>`, `/plan` (show),
  `/plan add <s>`, `/plan done <n>`, `/plan clear`. Surface in
  `SLASH_COMMANDS` + autocomplete.
- [ ] Inject before each model call in `process_turn`: one short system
  message (`name: Some("plan")`) with goal + unchecked steps. Suppress when
  empty. Route through `persist_pending` like the existing nudge.
- [ ] Model self-update: system prompt gains one line — "Maintain progress
  against the stated goal; state the current objective before non-obvious
  tool batches." (Prompt-level, not harness-parsed tool calls.)
- [ ] UI: one status-bar row in `ui/render.rs` — `Goal: … · Plan 3/7`, and a
  `SinkLine::Plan` event so remote clients (`ui/remote.rs`) stay in sync.
- **Accept:** `/goal` survives `/resume`; model receives goal each turn;
  status bar shows progress. Test: inject a plan, assert it appears in
  messages passed to the mock client.

## Phase 2 — Verification hook

Edits currently go unverified unless the model volunteers. Make verification
a harness reflex.

- [ ] Config: `verify_command: Option<String>` (and `OYE_VERIFY` env) in
  `LlmConfig`/`config.sample.json`. Explicit command first; auto-detection
  (cargo/go/package.json) is a later fallback, not now.
- [ ] In `process_turn`, after a batch containing a mutating call
  (`is_mutating` already exists): if `verify_command` is set and the
  iteration budget allows, run it through the existing `bash` tool path
  (same timeout/output caps/cancellation — no new execution code).
- [ ] Feed the result back:
  - pass → one-line `SinkLine::System` "verify ✓";
  - fail → tool-role message (`name: Some("verify")`) with the failure tail,
    so the next model call treats it as a high-priority observation. Do not
    count it against `max_tool_iterations` differently than any tool call.
- [ ] Deduplicate: skip re-running when no mutation happened since the last
  run (track a dirty flag in `ToolState`).
- **Accept:** with `verify_command = "cargo test"`, an edit triggers a run;
  a failing test reaches the model as an observation; Ctrl+C cancels it.
  Test: mock client performs an edit, executor runs the configured command
  against a temp workspace.

## Phase 3 — Turn-start context injection

Orientation snapshot so the model starts every turn with repository + task
state instead of rediscovering it.

- [ ] At turn start in `process_turn` (before the first model call only),
  inject one compact system message: goal/plan summary (Phase 1) +
  `git status --short` + `git diff --stat` when the workspace is a repo and
  output is non-empty. Reuse `tool_git` rather than shelling out.
- [ ] Cap it (~10 lines); omit silently outside a git repo.
- [ ] Compaction: extend the `summarize_old_messages` prompt
  (`agent/compaction.rs`) to preserve the current goal/plan verbatim and
  recent verification failures, so compaction cannot erase orientation.
- **Accept:** first model call of each turn sees goal + git snapshot; a
  compacted history still contains the goal. Test: mock client records
  messages; assert first request contains the injected block.

## Phase 4 — Stuck detection and escalation

Today only 3-identical-successful-calls are blocked. Extend the existing
`last_tools` ring buffer into a failure ledger.

- [ ] Track (ring buffer, same pattern as `last_tools`):
  - identical **failed** calls (currently only successes are counted),
  - repeated `edit`/`write` to the same path (3+ times),
  - `verify_command` failing with an unchanged failure signature
    (hash the first line of the failure),
  - N consecutive search calls (`grep`/`find`) with no `read` in between.
- [ ] On a pattern hit, inject the strategy-escalation nudge (reuse the
  `WRAP_UP_THRESHOLD` message shape): attempt 1 → direct fix; 2 → inspect
  surrounding architecture; 3 → question the assumption; 4+ → propose a
  re-plan and surface it to the user. Cap escalations per turn (e.g. 3) to
  avoid nudge spam.
- [ ] After the final escalation, abort the turn with the existing
  "budget exhausted, here's where we are" error — the recovery prompt to the
  user is already good.
- **Accept:** a mock model that repeats the same failing edit gets nudged,
  then the turn ends with a useful message; normal retry sequences are not
  blocked. Test each pattern with scripted mock responses.

## Phase 5 — Real capability discovery

`discover_capabilities` (`llm/client.rs`) hardcodes `streaming: true,
tools: true`.

- [ ] Per-provider capability table in `LlmConfig` (static, honest data —
  no runtime probing): streaming, tools, context window default.
- [ ] Use the context window to drive the existing compaction thresholds
  instead of requiring `context_window` to be configured by hand.
- **Accept:** unknown providers degrade to current behavior; known ones get
  correct compaction without manual config.

## Explicitly deferred (do not build now)

- **Subagents / parallel delegation** — a mediocre multi-agent system is
  worse than this single-agent loop; revisit only after Phases 1–4 are done.
- **AST/symbol tools (tree-sitter), LSP diagnostics** — large dependency,
  Tier-3 value; `grep`+`chain` cover most retrieval needs today.
- **`run_tests` as a dedicated tool** — `verify_command` (Phase 2) covers it;
  add a tool only if the model needs to choose targets per-turn.
- **Structured-output / reasoning-metadata normalization** — add when a
  concrete provider mismatch demands it; the `ModelClient` trait is the seam.
- **Automatic per-model context-limit discovery via probing** — static table
  is lazier and deterministic.

---

## Validation gates (unchanged, per existing practice)

Every phase: `cargo fmt -- --check`, `cargo test --all-targets`,
`cargo clippy --all-targets -- -D warnings`. Each phase lands with its own
tests before the next starts.

## Suggested order recap

| Phase | Ships | Effort |
|-------|-------|--------|
| 0 | Dead state restore wired | hours |
| 1 | Goal/plan persistence + injection + UI | 1–2 days |
| 2 | Verify-on-mutate hook | 1 day |
| 3 | Turn-start snapshot + compaction preserves orientation | 1 day |
| 4 | Failure-ledger stuck detection + escalation | 1–2 days |
| 5 | Static capability table | hours |
