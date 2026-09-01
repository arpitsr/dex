# Harness Plan — Next

`P0-P6`, `P8-P10` + task budget shipped. `HARNESS.md` now Direction 2/2/2, Trust `2/1/0/1`, Recovery Gate `2/2` — **Correctness gate closed** (`#1+#9+#10` now `2/2/2`), **Recovery gate closed** (`#7+#15` now `2/2`), **Remote still blocked by `#14 (0)`**; Improvement gate `#20` open but `#22` still `1` (no eval suite). Next remains Remote (#7 token auth + redaction) — it is the only `0` blocking release. Same primitives, no rewrites.

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
| 1b | Task contract level 2: `Plan` + `constraints`/`acceptance`/`is_complete`, `/constraint` `/accept` slash, full-contract `StreamEvent::Plan`, daemon validates plan JSON (fail-explicit), `plan_status` shows `✓ n/m` + `complete` | #1 `1→2` |
| 8 | Transactional edits (`expected_hash`→409) + change ledger + `/undo`; durable journal (`turn_start/complete/failed` + `effect_start/result`, `sync_data`); daemon registry rebuild + interrupted-turn marking; `io::Error` → `turn_failed` | #9 `1→2`, #15 `1→2`, #7 `1→2` |
| 9 | Verification disposition `pass|fail|waived` (+ auto-detect, `/waive`), redacted `trace.jsonl` (turn/llm/tool/verify spans, cost) + `GET /trace` | #10 `1→2`, #20 `1→2` |
| 10 | Versioned protocol (`Accept`/`X-Dex-Protocol`), `StreamEnvelope{seq}`, event journal + `/events?since=` replay, `Idempotency-Key` dedup, disk-backed `/sessions`, `--reattach` transcript replay | #17 `1→2`, #24 partial, #2 gap closed |
| 1c | Task budget in contract: `Plan.budget{max_seconds,max_tool_iterations,max_cost_usd}`, `/budget` slash, enforced by loop (tightest of config/budget), summary injection | #1 gap |

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

## Phase 8 — Change control & durability ✓ (closes #9, #7, #15 — recovery gate)

Edits must be reviewable and restart must not duplicate effects.

- [x] Transactional file tools: `write`/`edit` accept optional `expected_hash` → `StaleFile` (409 semantics) when the file moved since the read; every successful write/edit records `before/after` content (capped 64 KiB) + hashes in `session_state "changes"` (`session.rs record_change`/`make_change_record`); `SinkLine::System` diff preview before approval when `permission != trusted` (`loop.rs execute_tool_call` → `tools::change_preview`).
- [x] Durable journal: `turn_start`/`turn_complete`/`turn_failed` + `effect_start`/`effect_result` (`tool_call_id` + `input_hash`, `session.rs effect_start/effect_result`) appended via `File::sync_data`; `daemon::rebuild` restores the registry from JSONL, seeds per-session event cursors, and marks interrupted turns `turn_failed` with a `last_error` state; daemon `io::Error` on plan/turn_start/user-message/terminal markers propagates as `turn_failed`, not `let _ =`.
- [x] Checkpoint/undo: `/undo` (local `slash.rs` + `POST /api/sessions/{id}/undo`) restores the last change record, refusing when the file's `after_hash` no longer matches (409) or content was too large.
- **Accept:** stale `expected_hash` edit → `StaleFile` and file untouched (`write_edit_require_expected_hash_and_reject_stale`); `kill -9` daemon mid-turn → restart `rebuild` shows `turn_state: failed` + `last_error` marker (`rebuild_marks_interrupted_turns_failed_and_registers_sessions`, smoke-tested with curl); `/undo` restores after-content (`change_ledger_records_then_undo_restores`).

## Phase 9 — Verification gate + observability ✓ (closes #10, #20, starts #22)

"Done" must link claims to recorded checks; cost must be answerable without grepping raw logs.

- [x] Gate: mutating batch runs `verify_command` (config or auto-detect at daemon/one-shot boundary — `llm::config::detect_verify_command`, inside the loop only config is consumed) and persists `session_state "verify"` with `disposition: pass|fail` (+ tail hash). `/waive <reason>` (local `slash.rs` + `POST /api/sessions/{id}/waive`) records `disposition: waived` with a `name:waive` user message; empty reason → `400`. Fail → `name:verify` re-enters loop (P2 unchanged); verify runs inside the iteration budget (`iteration + 1 < iteration_cap`).
- [x] Trace: `Console.trace_span` writes redacted spans `{kind: turn|llm|tool|verify, turn_id, tool_call_id, duration_ms, prompt_tokens, cost_usd, ok, approval_required, effect_hash, verify}` to `<session>.trace.jsonl` (`0600`, `TraceWriter`); `GET /api/sessions/{id}/trace` returns rows. Cost = `prompt_tokens × DEX_COST_PER_1K` (default $2/M, approximate).
- **Accept:** `waived` without reason → `400` (smoke-tested); edit+verify pass persists `{"disposition":"pass"}` and trace contains `llm/tool/verify/turn` spans with no tool args (`effect_journal_verify_disposition_and_trace_are_written`); `grep trace.jsonl | jq .cost_usd` sums to task cost.

## Phase 10 — Protocol & reattach ✓ (closes #17, #24 partial, #2 gap)

Clients must reattach and replay deterministically.

- [x] Versioned protocol: `Accept: application/vnd.dex.v1+json` + `X-Dex-Protocol: 1` on every client `/api/*` call; server rejects unknown protocol on `/chat` (400). Each stream event is a `StreamEnvelope{seq, event}`; the daemon journals every event (incl. terminal + approval) to `<session>.events.jsonl` (`Session::append_event`/`load_events`) and `GET /api/sessions/{id}/events?since=seq` replays. `POST /chat` dedups `Idempotency-Key` for 60s per session+request-hash (`DaemonState::idempotent_replay/record`) and replays the recorded terminal envelope.
- [x] Reattach: `GET /api/sessions` lists persisted JSONL (created_at/message_count/turn_state); `POST /api/sessions/{id}/reattach` returns the replay cursor; `dex connect --reattach <id>` replays the journal into the transcript (`remote.rs`), skipping stale approvals.
- **Accept:** `curl /events?since=0` rehydrates a fresh client (smoke-tested); `--reattach` resumes the transcript; `Idempotency-Key` replay returns the recorded `turn_complete` without a second effect (unit: `idempotency_key_replays_same_turn_and_rejects_different_request`); restart seeds the cursor after the journal (`event_seq_is_seeded_from_disk_after_restart`).

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
