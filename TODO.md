# Coding Agent TODO

This repository has a solid prototype foundation, but it needs the following work before it is production-grade.

## Priority 1: Safety and reliability

- [x] Add workspace-aware path resolution and prevent unintended access outside the workspace.
- [x] Add configurable permission/approval modes:
  - [x] read-only
  - [x] ask before file writes
  - [x] ask before shell commands
  - [x] trusted/non-interactive mode
- [x] Add shell command wall-clock timeouts.
- [x] Add shell output byte limits and avoid buffering unlimited output in memory.
- [x] Add cancellation support for running shell processes, including process-group termination.
- [x] Add explicit HTTP connect, request, and streaming timeouts.
- [x] Make cancellation work while blocked in HTTP requests and tool execution.
- [x] Prevent conflicting parallel tool calls from racing:
  - [x] Parallelize read-only tools.
  - [x] Serialize writes and edits.
  - [x] Detect conflicting paths and preserve dependencies.
- [x] Remove or redesign the global tool cache:
  - [x] Scope entries by workspace/current directory.
  - [x] Include file modification metadata or content hashes.
  - [x] Avoid caching sensitive results by default.
  - [x] Add cache invalidation tests.
- [x] Add audit logging for commands, file mutations, approvals, and failures.

## Priority 2: Persistence and recovery

- [x] Persist session events incrementally instead of only at the end of a turn.
- [x] Record user messages immediately.
- [x] Record assistant responses, tool calls, and tool results as they occur.
- [x] Add turn-start, turn-complete, and turn-failed markers.
- [x] Ensure a crash loses no more than the currently incomplete event.
- [x] Persist slash-command state changes such as `/clear`, `/new`, model changes, and skill loads.
- [x] Make `/clear` durable across restarts.
- [x] Validate session versions and metadata when loading.
- [x] Improve session directory identifiers to avoid path collisions.
- [x] Implement `/resume <index|path>` instead of only listing sessions.

## Priority 3: Architecture and testability

- [x] Reduce the size of `src/main.rs` by extracting focused modules:
  - [x] `agent/loop.rs`
  - [x] `agent/state.rs`
  - [x] `agent/compaction.rs`
  - [x] `llm/client.rs`
  - [x] `llm/chat_completions.rs`
  - [x] `llm/responses.rs`
  - [x] `llm/streaming.rs`
  - [x] `tools/registry.rs`
  - [x] `tools/executor.rs`
  - [x] `tools/permissions.rs`
  - [x] `config/`
  - [x] `cli/`
- [x] Introduce traits/interfaces for mockable components:
  - [x] Model client.
  - [x] Tool executor.
  - [x] Session store.
  - [x] Clock/cancellation source where useful.
- [x] Add deterministic tests using mocked model responses and temporary workspaces.
- [x] Add tests for tool argument validation.
- [x] Add tests for exact-match edit behavior.
- [x] Add tests for shell escaping.
- [x] Add Chat Completions streaming parser fixtures.
- [x] Add Responses API streaming parser fixtures.
- [x] Add tests for tool-call aggregation and malformed streams.
- [x] Add retry and HTTP error tests.
- [x] Add cancellation and timeout tests.
- [x] Add session recovery and malformed-line tests.
- [x] Add compaction boundary and token-estimation tests.
- [x] Add CLI parsing tests.
- [x] Move tests before non-test items or otherwise make `cargo clippy --all-targets -- -D warnings` pass.
- [x] Make `cargo fmt -- --check` pass.
- [x] Add a `.gitignore` for `target/` and local/runtime files.

## Priority 4: Agent quality

- [x] Add metadata to each tool describing whether it is read-only, mutating, idempotent, and what permission level it requires.
- [x] Use structured tool errors instead of relying only on formatted strings.
- [x] Add robust handling for malformed or incomplete tool calls.
- [x] Add configurable per-turn limits for:
  - [x] Tool iterations.
  - [x] Prompt/context tokens.
  - [x] Tool output bytes.
  - [x] Total elapsed time.
- [x] Improve history compaction so it preserves tool-call/result pairs and important file facts.
- [x] Make compaction failures explicit and recoverable rather than silently losing context through truncation.
- [x] Add git diff/status inspection support.
- [x] Encourage verification in the agent instructions:
  - [x] Run relevant tests or checks.
  - [x] Inspect the resulting diff.
  - [x] Report validation results and unresolved issues.
- [x] Add retry handling for expired authentication and provider-specific errors.
- [x] Add model/provider capability discovery where possible.

## Priority 5: Skills, project context, and UX

- [x] Search parent directories for `AGENTS.md`/`CLAUDE.md` with clear precedence rules.
- [x] Make skills discovery deterministic by sorting directories and entries.
- [x] Validate skill names and reject duplicate/conflicting definitions clearly.
- [x] Persist loaded skills in session history consistently.
- [x] Improve `/resume` selection and session browsing.
- [x] Add a clear command to inspect permissions and current workspace.
- [x] Add visible approval prompts in the TUI.
- [x] Add non-interactive approval policies suitable for CI.
- [x] Improve terminal cleanup on panic, cancellation, and abnormal exit.
- [x] Add structured logs useful for debugging provider/API issues.

## Documentation and consistency

- [x] Fix the duplicate `--tool` entry in `README.md`.
- [x] Reconcile README defaults with code defaults, especially the API protocol.
- [x] Document security implications of `bash`, `write`, and `edit`.
- [x] Document timeout, output-limit, permission, and cache behavior.
- [x] Document session durability guarantees accurately.
- [x] Add an architecture overview and contributor guide.

## Current validation status

- [x] `cargo test --all-targets` currently passes.
- [x] `cargo clippy --all-targets -- -D warnings` currently passes.
- [x] `cargo fmt -- --check` currently passes.
- [x] Add CI to run formatting, tests, Clippy, and security/dependency checks.
