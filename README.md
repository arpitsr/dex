# dex

A terminal-based coding agent written in Rust. `dex` talks to OpenAI-compatible
Chat Completions or Responses APIs, calls tools (`read`, `bash`, `write`, `edit`, `ffgrep`,
`fffind`) to operate on your local files, and offers an interactive TUI, a
one-shot prompt mode, and a raw JSON tool mode. Conversations are persisted as
sessions and can be resumed.

## Features

- **OpenAI-compatible backend** — supports Chat Completions and Responses
  endpoints (OpenAI, OpenCode Zen, Moonshot/Kimi, etc.). Streaming responses,
  automatic retries with exponential backoff, and configurable reasoning effort.
- **Agentic tool use** — the model can read files, run shell commands, write
  and edit files, search the filesystem. Extra tools (`git`, `chain`) behind `DEX_EXTRA_TOOLS=1`. Tool output caching disabled by default; set `DEX_TOOL_CACHE=1` to opt in. Pi-fast defaults: minimal prompt, parallel `write`/`edit` on distinct files, deterministic compaction, no per-turn `git`/`verify` tax.
- **Interactive TUI** — a `ratatui` REPL with a streaming markdown transcript
  (via `ratatui-markdown`), a custom multi-line input editor with an inline
  block cursor and soft-wrapping (no `tui-textarea` underline / horizontal
  overflow), a solid-bordered input box, autoscroll, a status bar, and a
  visible steering/follow-up queue while the agent is working.
- **Session persistence** — each conversation is saved as a JSONL log. A fresh
  session starts by default; use `--session` to explicitly continue one.
- **Skills** — lightweight, discoverable agent skills (directories with a
  `SKILL.md` frontmatter) can be injected into the system prompt or loaded on
  demand via `/skill:<name>`.
- **History compaction** — when the context window is exceeded, older turns are summarized deterministically (no LLM call) to keep requests bounded. Set `DEX_COMPACTION_LLM=1` for model summarization.
- **Project instructions** — a repo-level `AGENTS.md`/`CLAUDE.md` is appended to
  the system prompt automatically.

## Building

Requires a Rust toolchain (edition 2021):

```sh
cargo build --release
# binary: target/release/dex
```

## Configuration

There is no config file — everything is environment variables (plus `--model` /
`--base-url` CLI flags and `/model` + `/provider` session switches, which persist
across resumes via session state). To get started:

```sh
export OPENAI_API_KEY=sk-...                    # the only required setting
# OpenCode Zen instead of api.openai.com:
export OPENAI_BASE_URL=https://opencode.ai/zen/v1
# which talks completions for some models:
export DEX_MODEL_APIS=kimi-k2.6=openai-completions
dex
```

The model carries its own wire protocol: `/model` switches it automatically
(e.g. `kimi-k2.6` speaks `openai-completions` while `gpt-5.6-luna` speaks
`openai-responses` on the same provider), resolved from `DEX_MODEL_APIS` by bare
id or full `endpoint/id` selection (full selection wins). `OPENAI_API` pins one
protocol for everything when set.

For ChatGPT-backed Codex, first run `codex --login`, then:

```sh
DEX_PROVIDER=openai-codex dex
```

`dex` reads the current access token and account ID from
`CODEX_ACCESS_TOKEN`/`CODEX_ACCOUNT_ID` or `$CODEX_HOME/auth.json`
(default `~/.codex/auth.json`). Run `codex --login` again when the local token
expires.

If a legacy `~/.config/dex/config.yaml` exists, dex prints a warning and
ignores it — translate its fields to the variables below and delete it.

## Usage

Run `dex` with no arguments to launch the interactive TUI:

```sh
dex
```

Ask it to do something:

```
> read src/main.rs and summarize what it does
```

### One-shot mode

Pass a prompt as arguments to get a single answer (no TUI):

```sh
dex "explain the Cargo.toml dependencies"
```

### Raw tool mode

`dex --tool` reads JSON tool-invocation lines from stdin and prints JSON
results. Useful for piping tool calls from another process:

```sh
echo '{"name":"read","args":{"path":"Cargo.toml"}}' | dex --tool
# => {"ok":"[package]\nname = \"dex\"\n..."}
```

An empty line quits raw tool mode.

### One-shot tool mode

`dex run <tool> <key>=<value>...` executes a single tool and prints the raw
result (errors go to stderr with exit code 1). Values that look like JSON
numbers/booleans are coerced (`limit=5`, `replaceAll=true`); a single JSON
object string is also accepted. Inside agent shell commands the binary is
available as `$DEX_BIN`, so ONE bash call can stitch a whole read-only
pipeline — search locally, read excerpts, print only the distilled result —
while intermediate output never enters the conversation:

```sh
"$DEX_BIN" run ffgrep pattern=TODO output_mode=files | while IFS= read -r f; do
  "$DEX_BIN" run read "path=$f" limit=3
done
```

### Model catalog

`dex update --models` refreshes the cached model catalog (context windows
and `/model` autocomplete; like `pi update --models`).

```sh
dex update --models
```

### Client–server mode

The TUI is a pure HTTP client; all agent work (LLM calls, tools, sessions)
happens in a daemon. `dex` with no arguments starts a daemon in the
background and attaches the TUI to it:

```sh
dex                     # daemon on a random localhost port + TUI
```

Run the daemon headless (e.g. on a remote machine, in the directory you want
as the agent workspace) and connect the TUI from anywhere:

```sh
dex serve               # daemon on 127.0.0.1:8420
dex serve 0.0.0.0:8420  # reachable from other machines
dex connect http://127.0.0.1:8420
dex connect http://10.0.0.5:8420 "explain the Cargo.toml dependencies"  # one-shot
```

The TUI behaves exactly like the local one: assistant text streams live,
tool calls and results appear as they happen, tool approvals pop up as an
overlay (the daemon parks the turn until you decide), and Ctrl+C/Esc cancels
the in-flight turn. API keys, the model, and the permission mode are
resolved by the daemon's own environment; client flags like
`--model` and `--permission` are forwarded as per-request overrides.

Note that tools execute on the machine where the daemon runs, confined to
the daemon's working directory.

## Command-line flags

| Flag               | Description                                              |
| ------------------ | -------------------------------------------------------- |
| `--base-url <url>` | Override the API base URL for this run.                  |
| `--model <name>`   | Override the model for this run.                         |
| `-s`, `--session <path>` | Open/continue a specific session file.             |
| `--no-session`     | Disable session persistence for this run.                |
| `-n`, `--new`      | Start a new session (the default).                       |
| `--permission <mode>` | Tool permissions: `read-only`, `ask-writes`, `ask-shell`, or `trusted` (default `trusted`). |
| `--name <name>`    | Name the session.                                    |
| `--reattach <id>`  | Attach to an existing daemon session and replay its event journal. |
| `--skill <dir>`    | Add an extra skill directory to discover skills from.    |
| `--tool`           | Run raw JSON tool mode (read JSON lines from stdin).     |

Tool safety defaults to `trusted` (no approval popups). Set `DEX_PERMISSION=ask-writes`
or pass `--permission` to approve writes and shell commands. Paths are confined to the current
workspace; `bash` can execute arbitrary commands in that workspace and should
only be enabled in trusted environments. Shell commands default to 120
seconds and 1 MiB per output stream. HTTP requests default to 10 seconds to
connect and 300 seconds overall. Sessions journal to `$XDG_DATA_HOME/dex/sessions/*.jsonl` with `fsync` only on `turn_*`/`effect_*` (set `DEX_DURABLE=1` for per-line). Audit to `audit.jsonl` is off by default (`DEX_AUDIT=1` to enable). Configure limits with `DEX_TOOL_*` and `DEX_HTTP_*` environment variables
(see table below).

In the interactive TUI, actions requiring approval open a dedicated overlay.
Use the arrow keys and Enter to choose `Allow once`, `Allow for this session`,
or `Deny`; `y`, `s`, and `n` are direct shortcuts, and Esc denies.

Any other arguments are treated as a one-shot prompt.

## TUI slash commands

| Command             | Description                                          |
| ------------------- | ---------------------------------------------------- |
| `/quit`             | Exit the REPL.                                       |
| `/permissions`      | Show permission mode and workspace.                  |
| `/clear`            | Clear the conversation history (keeps the system prompt). |
| `/new`              | Start a new session and clear history.               |
| `/session`          | Show the current session id, path, and turn count.   |
| `/resume [index|path]` | List sessions, or resume one by index/path.       |
| `/name <name>`      | Rename the current session.                          |
| `/skill:<name>`     | Load a skill's full content into the conversation.    |
| `/model`           | Show the current model and wire protocol.            |
| `/model <name>`     | Switch the model for the rest of the session.         |
| `/waive <reason>`   | Waive verification with a reason.                    |
| `/undo`             | Undo the last recorded file change.                  |
| `/provider`        | Show the current and available providers.             |
| `/provider <name>` | Switch provider for the rest of the session.          |
| `/help` (unknown)   | Unknown commands print a hint.                        |

### Keyboard controls

- **Enter** — submit the current input.
- **Tab** — autocomplete the selected slash command, provider, or model; **↑/↓** navigate suggestions.
- **Shift+Enter** — insert a newline (multi-line input).
- **Enter while working** — queue a steering message for the next model boundary.
- **Alt+Enter while working** — queue a follow-up for after the current task.
- **Esc** or **Ctrl+C** — cancel the active turn and restore queued messages.
- **Ctrl+T** — expand/collapse the full thinking block.
- **PageUp/PageDown**, **Shift+Up/Down**, or **mouse wheel** — scroll the transcript.
- **Paste** — pasted text is inserted at the cursor.
- **Mouse drag** — selects text natively for copying; the TUI does not capture the mouse.

### Steering and follow-ups

While `dex` is working, the input remains available. Submitted steering and
follow-up messages stay visible in the queue directly above the input box until
the worker accepts them. Steering is delivered before the next model call;
follow-ups wait until the current task has finished. The queue is kept separate
from the transcript so pending messages do not scroll away.

## Sessions

Sessions are stored as JSONL files under:

- `$XDG_DATA_HOME/dex/sessions` (or `~/.local/share/dex/sessions`),
- organized in subdirectories by a slug of the current working directory.

Each file starts with a `session` header line followed by `message` entries and
optional `session_info` (rename) entries. Entries are appended after every
turn, so a crash or Ctrl+C loses at most the in-progress turn. Starting `dex` in
a directory creates a fresh session; use `--session <path>` to continue a saved
one.

## Skills

Skills are discovered from these directories (first match wins per directory):

- `<cwd>/.dex/skills`
- `<cwd>/.agents/skills`
- `$XDG_CONFIG_HOME/dex/skills` (or `~/.config/dex/skills`)

A skill is a directory containing a `SKILL.md` file with YAML frontmatter:

```markdown
---
name: my-skill
description: Short description surfaced to the model.
---

Detailed instructions / reference content...
```

Only the `name` and `description` are included in the system prompt; the full
body is loaded on demand via `/skill:<name>` or when the conversation
references it.

## Tools

The agent can call the following tools (each maps to a function in the API
schema):

| Tool    | Purpose                                                          |
| ------- | -----------------------------------------------------------------|
| `read`  | Read a file (`path`, `paths`, `glob`). Line-numbered, tab-expanded. |
| `bash`  | Run a shell command via `sh -c` (`command`).                     |
| `write` | Write/overwrite a file (`path`, `content`).                      |
| `edit`  | Replace exactly one occurrence of text (`path`, `oldText`, `newText`). |
| `ffgrep` | Fast frecency-ranked content search (fff engine): regex or plain text, typo-tolerant fuzzy fallback, respects `.gitignore` (`pattern`, `output_mode`). |
| `fffind` | Fuzzy frecency-ranked file-path search (fff engine, typo-tolerant) (`pattern`, `limit`). |
| `git`*   | Inspect repo status/diff (`mode`). Behind `DEX_EXTRA_TOOLS=1`.   |
| `chain`* | Bounded read-only search→read in one round trip. Behind `DEX_EXTRA_TOOLS=1`. |

`*` behind `DEX_EXTRA_TOOLS=1` — pi parity is 6 tools. Tool results are truncated before being sent back to the model, and a result
cache (`dex-tool-cache.json`) is kept across runs to reduce redundant work. `write`/`edit` on distinct files run in parallel; same `path` or any `bash` still serializes.

## Environment variables

| Variable             | Description                                              |
| -------------------- | -------------------------------------------------------- |
| `OPENAI_API_KEY`     | API key (required for `opencode`; export it in your shell profile). |
| `OPENAI_BASE_URL`    | API base URL (default `https://api.openai.com/v1`; e.g. `https://opencode.ai/zen/v1`). |
| `OPENAI_MODEL`       | Model selection (default `gpt-5.6-luna`).                |
| `OPENAI_API`         | Wire protocol default (`openai-completions` or `openai-responses`); pins one protocol for everything. |
| `DEX_PROVIDER`        | Provider selection (`opencode` or `openai-codex`, default `opencode`). |
| `CODEX_ACCESS_TOKEN` | Optional Codex OAuth access-token override.                |
| `CODEX_ACCOUNT_ID`   | Account ID paired with `CODEX_ACCESS_TOKEN`.               |
| `DEX_MODELS` | Comma-separated models for `/model` autocomplete (default: catalog cache). |
| `DEX_HTTP_CONNECT_TIMEOUT_SECS` | HTTP connect timeout (default 10). |
| `DEX_HTTP_REQUEST_TIMEOUT_SECS` | HTTP per-read timeout (default 300; streaming-safe). |
| `DEX_TOOL_TIMEOUT_SECS` | Shell command timeout in seconds (default 120). |
| `DEX_TOOL_OUTPUT_BYTES` | Maximum captured stdout/stderr bytes per stream (default 1 MiB). |
| `DEX_MODEL_APIS` | Per-model wire protocol table (`id=api,...`; full `endpoint/id` key beats bare id). |
| `DEX_THINKING_EFFORT` | Reasoning effort passed to the API (e.g. `medium`). |
| `DEX_PERMISSION` | Tool permission mode (`read-only`, `ask-writes`, `ask-shell`, or `trusted`; default `trusted`). |
| `DEX_VERIFY`    | Verification hook: `1` auto-detects `cargo test`/`go test`/`npm test`; or set to a command. Off by default (pi has no verify). |
| `DEX_COMPACTION_LLM` | `1` to use LLM summarization for compaction (default deterministic). |
| `DEX_DURABLE`   | `1` to `fsync` every session line (default only `turn_*`/`effect_*`). |
| `DEX_AUDIT`     | `1` to write `audit.jsonl` per tool call (default off; session already journals). |
| `DEX_EXTRA_TOOLS` | `1` to expose `git`+`chain` to the model (default 6 tools). |
| `DEX_COST_PER_1K` | Prompt cost per 1k tok for `trace.jsonl` (default `0.002`). |
| `DEX_CONTEXT_WINDOW` | Override model context window (pi: per-model from catalog, e.g. gpt-5.6 1050000, claude 200k, muse 1048576). |
| `DEX_RESERVE_TOKENS` | Tokens reserved for reply (default 16384, pi: `compaction.reserveTokens`). |
| `DEX_KEEP_RECENT_TOKENS` | Recent tokens kept on compaction (default 20000, pi: `compaction.keepRecentTokens`). |
| `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_CACHE_HOME` | XDG base dirs for config/data/cache. |
| `HOME`               | Fallback when XDG vars are unset.                        |

Restore strict harness: `DEX_DURABLE=1 DEX_AUDIT=1 DEX_EXTRA_TOOLS=1 DEX_VERIFY=1 DEX_COMPACTION_LLM=1 dex`

## Project structure

```
dex/
├── .dex/
│   └── skills/           # (optional) project-level agent skills
├── target/
├── Cargo.lock
├── Cargo.toml
├── README.md
└── src/
    ├── main.rs           # entry point: mode resolution, daemon bootstrap
    ├── cli.rs            # argument parsing / invocation mode
    ├── protocol/         # client<->daemon wire types (HTTP + SSE events)
    ├── client/           # HTTP client: SSE turn streaming, approvals, REPL
    ├── daemon/           # axum daemon: sessions, chat SSE, approve, cancel
    ├── agent/            # turn loop, steering, compaction, tool state
    ├── core/             # console sinks, formatting, highlighting, types
    ├── llm/              # provider clients, streaming parsers, auth, config
    ├── session.rs        # JSONL session persistence
    ├── tools/            # builtin tools: read, bash, write, edit, ffgrep, fffind (fff engine)
    ├── skills.rs         # skill discovery
    └── ui/               # ratatui TUI (local event loop + remote client UI)
```

## How it works

A turn runs in `src/agent/loop.rs` (`process_turn`): it repeatedly calls the
model with tools enabled, executes any requested tool calls in parallel ( `write`/`edit` on distinct files in parallel; `bash` or same `path` serializes), feeds
results back, and compacts history deterministically once `tokens > contextWindow - reserveTokens` (pi: `reserve=16384`, `keepRecent=20000` tokens, per-model `contextWindow` from catalog/`DEX_CONTEXT_WINDOW`). The optional `DEX_VERIFY` hook is off by default for pi-fast latency. Progress is reported through a `Console` (streamed lines + approval
requests).

In client–server mode the daemon runs `process_turn` on a blocking thread and
translates console output into `StreamEvent`s over SSE
(`src/daemon/server.rs`). The TUI (`src/ui/remote.rs`) consumes those events
from a worker thread and renders them live; approvals and cancellation are
round-tripped over `POST .../approve` and `POST .../cancel`. The local TUI
(`src/ui/event.rs`) runs the same loop in-process with direct channels.

## License

See the repository for license information.
