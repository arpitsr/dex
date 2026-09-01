# Harness Plan — Next

`P0-P5` shipped (intelligence: goal/plan, verify, context, stuck, caps). `HARNESS.md` now `1/2/1/1` for Direction/Knowledge — Trust/Correctness/Recovery still `1/0` and block release. Next closes the **release gates** in order: Remote → Correctness → Recovery → Improvement. Same primitives, no rewrites.

## Principles (still)

- **Injection over state machine.** New behavior = `system` message at existing seam (`WRAP_UP_THRESHOLD` template).
- **State in daemon, persisted in JSONL.** `Session::set_state` is the store; `load_session_state` is the restore.
- **Ceiling, not flag.** Daemon policy beats client request. Fail closed.
- **One phase ships.** Independently useful, tested, then next.

---

## Shipped — P0-P5 ✓ (see git log)

| Phase | What | HARNESS |
|---|---|---|
| 0 | `load_session_state` wired on `/resume`, `plan`/`model`/`provider` restore | #7 `1` |
| 1 | `Plan{goal,steps}` `session.set_state("plan")`, `/goal` `/plan add/done/clear`, `name:plan` inject each turn + `SinkLine::Plan`→`StreamEvent::Plan`, `Goal:·Plan 3/7` bar | #1 `0→1`, #16 |
| 2 | `verify_command`/`DEX_VERIFY`, `ToolState::verify_dirty`, `bash` same caps/cancel, `verify ✓` / `name:verify` fail, dedup | #10 `0→1`, #6 partial |
| 3 | `turn_start_context` once/turn `Plan+git status/diff --stat` cap 10 + `summarize_old_messages` preserves plan/verify | #5 `1→2` |
| 4 | `last_failed`/`edit_paths`/`last_verify_hash`/`search_streak` + `stuck_nudge` 1-4 cap 3 → abort | #3 `1→2` |
| 5 | `provider_default_context_window` + honest `discover_capabilities` drives `context_window/2` compaction | #19 `#21` |

---

## Phase 6 — Permission ceiling & audit (closes #12) ✓

Client must not escalate daemon policy. One approval must not become a shell wildcard.

- [x] Daemon owns ceiling: `LlmConfig::permission` from file/env is max; `ChatRequest.permission` may only request **equal or stricter** (`read-only > ask-writes > ask-shell > trusted`). Reject `trusted` override with `403` + `StreamEvent::Error`. (`daemon/server::run_turn_inner` checks `daemon_perm.permissiveness()` vs `req_perm`, `PermissionMode::permissiveness()` in `core/types`)
- [x] Scope approvals: approval key = `name + hash(input+diff)` not just `name`. `write`/`edit` scoped to `path`, `bash` scoped to `command` hash. Expiry 10m / one turn (per-turn `Console` drop). `Session` approval set stores hash, not string. (`core/console::approval_key`, `session_approved`/`record_session_approval` scoped)
- [x] Audit: `session_state` + `audit.jsonl` record `actor` (`local`/`remote`), `request_id`, `decision` (`once`/`session`/`deny`), `input_hash`, timestamp. (`agent/loop::audit_approval`, `daemon/server::approve` audit block writes `audit.jsonl` `0600`)
- **Accept:** `DEX_PERMISSION=ask-writes` daemon, client `ChatRequest{permission:"trusted"}` → `403`; `approve bash "echo hi"` does not approve `bash "rm -rf /"`; `grep audit.jsonl` shows `allow-session` with hash.

## Phase 7 — Secrets & remote security (closes #14, unblocks remote gate)

Unauthenticated `HTTP + bash` is RCE. Make `store:false` + redaction the default.

- [ ] Daemon auth: `DEX_TOKEN` file (`~/.config/dex/token`, `0600`) or env, `Authorization: Bearer <token>` on every `/api/*` (health exempt). `cargo` generates token on first `dex serve` if missing, prints `dex connect http://host:port?token=…`.
- [ ] Server policy: `base_url`/`permission` overrides forbidden for remote (only `model`/`skill_dirs` may be forwarded). `ApiProtocol` forced server-side.
- [ ] Redaction + retention: `session.rs`/`audit.jsonl`/`provider.jsonl` redact `api_key`, `base_url` secrets, `write` content >200 chars truncated in logs; file mode `0600`; `DEX_RETENTION_DAYS` (default 30) prunes old sessions.
- **Accept:** `curl /api/sessions` without token → `401`; `curl -H "Authorization: Bearer bad"` + `base_url: https://evil.com` → `403` and daemon `base_url` unchanged; `grep -r AKIA` in `~/.local/share/dex/` finds nothing.

## Phase 8 — Change control & durability (closes #9, #7, #15 — recovery gate)

Edits must be reviewable and restart must not duplicate effects.

- [ ] Transactional file tools: `write`/`edit` require `expected_hash` (from `read` preview) → `409` if stale; record `before_hash`/`after_hash`/`patch` in `session_state "change:<id>"`; `SinkLine::System` patch preview before approval when `permission != trusted`.
- [ ] Durable journal: `turn_start`/`turn_complete`/`turn_failed` + `effect_start`/`effect_result` (`tool_call_id` + `hash`) appended via `fsync` alternative (`File::sync_data`); `daemon::run_daemon` rebuilds `sessions` + `active_turns` on restart from JSONL; persistence `io::Error` → `turn_failed` not ignored.
- [ ] Checkpoint/undo: `session::clear` keeps `before` snapshot; `/undo` (or `edit` with `hash` mismatch) restores last change set.
- **Accept:** two `edit` to same file concurrent → second `409`; `kill -9` daemon mid-`bash` → restart `load_session_state` shows `turn_failed` with `unknown` effects reconciled, no duplicate `write`.

## Phase 9 — Verification gate + observability (closes #10, #20, starts #22)

"Done" must link claims to recorded checks; cost must be answerable without grepping raw logs.

- [ ] Gate: every mutating batch gets `verification_disposition: pass|fail|waived` in `Session` (`waived` requires `name:waive` reason). `verify_command` auto-detect fallback (`cargo test`/`go test`/`npm test` if file exists) when not set. Fail → `name:verify` re-enters loop; `max_tool_iterations` includes verify round.
- [ ] Trace: `SinkLine` already has `Usage/ToolOutput`; add `TraceSpan{task_id,turn_id,tool_call_id, start, duration, tokens, cost, approval, effect_hash, verify}` redacted, single `trace.jsonl` per turn, `0600`. `GET /api/sessions/{id}/trace` returns it.
- **Accept:** `DEX_VERIFY="cargo test"` edit → failing test → next model sees `name:verify` + trace shows `verify:fail cost:$0.02`; `waived` without reason → `400`; `grep trace.jsonl | jq .cost` sums to task cost.

## Phase 10 — Protocol & reattach (closes #17, #24, #2 gap)

Clients must reattach and replay deterministically.

- [ ] Versioned protocol: `Accept: application/vnd.dex.v1+json` + `X-Dex-Protocol: 1`; `StreamEvent` gains `seq: u64`; `GET /api/sessions/{id}/events?since=seq` replays; `POST /api/sessions/{id}/chat` idempotent via `Idempotency-Key` header ( dedup on `prompt_hash` 60s).
- [ ] Reattach: `GET /api/sessions` now lists persisted JSONL sessions (not just in-memory `sessions` Mutex), `POST /api/sessions/{id}/reattach` returns `seq` cursor; TUI `run_ratatui_repl_with_remote` on reconnect replays missed `seq` then streams new.
- **Accept:** disconnect mid-turn (kill SSE), `curl /events?since=last_seq` returns missed `tool_result`+`turn_complete`; `dex connect --reattach <id>` resumes same transcript; `Idempotency-Key` replay returns same `turn_complete` without second `bash` run.

## Explicitly deferred (still)

- **Subagents / delegation (#11, #25)** — needs #9/#12/#15/#20 guarantees first; single-agent loop is the scale gate.
- **LSP/diagnostics (#6 Tier-3)** — `grep`+`chain` still covers 90%; add only on demonstrated retrieval failure with eval.
- **Sandbox hard-isolation (#13 full)** — `bwrap`/nsjail per-turn mount+net+quota is correct but is a new binary/privilege; do `Phase 6-8` cheap confinement first, then measure.
- **Instruction hierarchy (#4) full + repo map (#6)** — needs provenance labels + cached map invalidation; do after traces (#9) exist to measure.
- **Long-running autonomy (#25) queue/leases/heartbeat** — requires #7+#15 durable journal.

---

## Validation gates (same)

Every phase: `cargo fmt -- --check`, `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`. Each lands with its own `behavioral` test (mock `ModelClient` + temp workspace + `NeverCancel`) before next starts. `HARNESS.md` re-scored after phase with code ref + acceptance check.

## Order recap (next)

| Phase | Ships | Effort | Gate |
|---|---|---|---|
| 6 | Permission ceiling + scoped audit | 1 day | Remote |
| 7 | Token auth + server policy + redaction | 1-2 days | Remote |
| 8 | Transactional edits + durable journal + rebuild | 1-2 days | Recovery |
| 9 | Verification disposition + redacted trace/cost | 1 day | Correctness+Improvement |
| 10 | Versioned protocol + seq/replay/reattach | 1 day | Recovery+Interface |
