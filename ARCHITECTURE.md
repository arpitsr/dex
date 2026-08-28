# Architecture

`main.rs` owns configuration, provider wire formats, streaming, the agent
loop, and the command-line entry point. `tools.rs` contains the workspace
confined tool implementations; `session.rs` owns append-only JSONL persistence;
`ui.rs` is the terminal front end and runs turns on a worker thread.

Tool calls are validated and permission-checked before execution. Read-only
calls may be safely retried; writes and shell commands are treated as
mutations. Sessions append each completed message and a `clear` marker, so a
crash can leave at most the currently incomplete operation unsaved.

For contributors, run `cargo fmt`, `cargo test --all-targets`, and
`cargo clippy --all-targets -- -D warnings` before submitting changes.
