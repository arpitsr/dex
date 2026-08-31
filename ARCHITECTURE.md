# Architecture

The binary is split into a client and a daemon that speak HTTP+ SSE
(`src/protocol`). The TUI (`src/ui`) is a pure rendering/event-loop front
end: a per-turn worker thread streams `StreamEvent`s from the daemon while
the UI keeps redrawing, and tool approvals travel back as `POST /approve`
decisions. The daemon (`src/daemon`) owns all agent work — `main.rs` owns
configuration, provider wire formats, and the command-line entry point;
`agent/loop.rs` runs the turn loop; `tools.rs` contains the workspace
confined tool implementations; `session.rs` owns append-only JSONL
persistence.

`oye` (no args) starts a daemon on a background thread and attaches the TUI
to it; `oye serve [host:port]` runs the daemon headless and `oye connect
<url>` attaches a TUI to a remote one. Tool approvals are parked server-side
keyed by request id, streamed to the client as `ApprovalRequired` events, and
resolved by `POST /api/sessions/{id}/approve`. Cancellation propagates via
`POST /api/sessions/{id}/cancel`.

Tool calls are validated and permission-checked before execution. Read-only
calls may be safely retried; writes and shell commands are treated as
mutations. Sessions append each completed message and a `clear` marker, so a
crash can leave at most the currently incomplete operation unsaved.

For contributors, run `cargo fmt`, `cargo test --all-targets`, and
`cargo clippy --all-targets -- -D warnings` before submitting changes.
