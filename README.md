# dex

A terminal-based coding agent written in Rust. `dex` talks to OpenAI-compatible
Chat Completions or Responses APIs, calls tools (`read`, `bash`, `write`, `edit`, `grep`,
`find`) to operate on your local files, and offers an interactive TUI, a
one-shot prompt mode, and a raw JSON tool mode. Conversations are persisted as
sessions and can be resumed.

## Features

- **OpenAI-compatible backend** — supports Chat Completions and Responses
  endpoints (OpenAI, OpenCode Zen, Moonshot/Kimi, etc.). Streaming responses,
  automatic retries with exponential backoff, and configurable reasoning effort.
- **Agentic tool use** — the model can read files, run shell commands, write
  and edit files, search the filesystem, and inspect git status/diffs. Tool
  output caching is disabled by default; set `DEX_TOOL_CACHE=1` to opt in.
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
- **History compaction** — when the context window is exceeded, older turns are
  summarized (or truncated as a fallback) to keep requests bounded.
- **Project instructions** — a repo-level `AGENTS.md`/`CLAUDE.md` is appended to
  the system prompt automatically.

## Building

Requires a Rust toolchain (edition 2021):

```sh
cargo build --release
# binary: target/release/dex
```

## Configuration

Configuration is read from a JSON file. The path is resolved in this order:

1. `$DEX_CONFIG` (if set)
2. `$XDG_CONFIG_HOME/dex/config.json`
3. `~/.config/dex/config.json`

Copy the sample to get started:

```sh
mkdir -p ~/.config/dex
cp config.sample.json ~/.config/dex/config.json
```

`config.json` fields:

| Field            | Type     | Description                                                        |
| ---------------- | -------- | ------------------------------------------------------------------ |
| `provider`       | string   | `opencode` or `openai-codex`.                                     |
| `api_key`        | string   | Default API key (overridable by `OPENAI_API_KEY`).                 |
| `base_url`       | string   | Default API base URL (overridable by `OPENAI_BASE_URL`).           |
| `model`          | string   | Default model (overridable by `OPENAI_MODEL`).                     |
| `models`         | string[] | Models shown by `/model` autocomplete (also configurable via `DEX_MODELS`, comma-separated). |
| `api`            | string   | Wire protocol: `openai-completions` or `openai-responses` (overridable by `OPENAI_API`). |
| `thinking_effort`| string   | Optional reasoning effort passed to the API (e.g. `"medium"`).     |
| `context_window` | integer  | Token context window used for compaction/status (overridable by `DEX_CONTEXT_WINDOW`). |
| `max_tool_iterations` / `max_prompt_tokens` / `max_tool_output_bytes` / `max_turn_seconds` | integer | Per-turn safety limits. |
| `http_connect_timeout_secs` / `http_request_timeout_secs` | integer | HTTP connection and request limits. |

The `api` field follows the provider/model API distinction used by Pi and Codex. It defaults to `openai-responses`.

For OpenCode, use its API key and endpoint. For ChatGPT-backed Codex, first run
`codex --login`, then select the Codex provider:

```json
{
  "provider": "openai-codex",
  "model": "gpt-5.6-luna",
  "api": "openai-responses"
}
```

When `provider` is `openai-codex`, `dex` reads the current access token and
account ID from `CODEX_ACCESS_TOKEN`/`CODEX_ACCOUNT_ID` or
`$CODEX_HOME/auth.json` (default `~/.codex/auth.json`). Run `codex --login`
again when the local token expires.

A minimal example:

```json
{
  "api_key": "sk-...",
  "base_url": "https://opencode.ai/zen/v1",
  "model": "gpt-5.6-luna",
  "api": "openai-responses"
}
```

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
"$DEX_BIN" run grep pattern=TODO output_mode=files | while IFS= read -r f; do
  "$DEX_BIN" run read "path=$f" limit=3
done
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
resolved by the daemon's own environment/config file; client flags like
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
| `--permission <mode>` | Tool permissions: `read-only`, `ask-writes`, `ask-shell`, or `trusted`. |
| `--skill <dir>`    | Add an extra skill directory to discover skills from.    |
| `--tool`           | Run raw JSON tool mode (read JSON lines from stdin).     |

Tool safety defaults to `ask-writes`. Paths are confined to the current
workspace; `bash` can execute arbitrary commands in that workspace and should
only be enabled in trusted environments. Shell commands default to 120
seconds and 1 MiB per output stream. HTTP requests default to 10 seconds to
connect and 300 seconds overall. Tool calls are recorded in
`$XDG_DATA_HOME/dex/audit.jsonl` (or the equivalent path under
`~/.local/share`). Configure limits with `DEX_TOOL_*`, `DEX_HTTP_*`, and
`DEX_MAX_*` environment variables or the corresponding JSON config fields.

In the interactive TUI, actions requiring approval open a dedicated overlay.
Use the arrow keys and Enter to choose `Allow once`, `Allow for this session`,
or `Deny`; `y`, `s`, and `n` are direct shortcuts, and Esc denies.

Any other arguments are treated as a one-shot prompt.

## TUI slash commands

| Command             | Description                                          |
| ------------------- | ---------------------------------------------------- |
| `/quit`             | Exit the REPL.                                       |
| `/clear`            | Clear the conversation history (keeps the system prompt). |
| `/new`              | Start a new session and clear history.               |
| `/session`          | Show the current session id, path, and turn count.   |
| `/resume [index|path]` | List sessions, or resume one by index/path.       |
| `/name <name>`      | Rename the current session.                          |
| `/skill:<name>`     | Load a skill's full content into the conversation.    |
| `/model`           | Show the current model.                               |
| `/model <name>`     | Switch the model for the rest of the session.         |
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
| `read`  | Read a file (`path`).                                            |
| `bash`  | Run a shell command via `sh -c` (`command`).                     |
| `write` | Write/overwrite a file (`path`, `content`).                      |
| `edit`  | Replace exactly one occurrence of text (`path`, `oldText`, `newText`). |
| `grep`  | Search file contents recursively for a literal pattern (`pattern`, `path`). |
| `find`  | Find file paths matching a pattern (`pattern`, `path`).          |

Tool results are truncated before being sent back to the model, and a result
cache (`dex-tool-cache.json`) is kept across runs to reduce redundant work.

## Environment variables

| Variable             | Description                                              |
| -------------------- | -------------------------------------------------------- |
| `OPENAI_API_KEY`     | API key (takes precedence over `config.json`).           |
| `OPENAI_BASE_URL`    | API base URL override (non-empty).                       |
| `OPENAI_MODEL`       | Model override.                                          |
| `OPENAI_API`         | Wire protocol override (`openai-completions` or `openai-responses`). |
| `DEX_PROVIDER`        | Provider override (`opencode` or `openai-codex`).          |
| `CODEX_ACCESS_TOKEN` | Optional Codex OAuth access-token override.                |
| `CODEX_ACCOUNT_ID`   | Account ID paired with `CODEX_ACCESS_TOKEN`.               |
| `DEX_TOOL_TIMEOUT_SECS` | Shell command timeout in seconds (default 120). |
| `DEX_TOOL_OUTPUT_BYTES` | Maximum captured stdout/stderr bytes per stream (default 1 MiB). |
| `DEX_PERMISSION` | Tool permission mode (`read-only`, `ask-writes`, `ask-shell`, or `trusted`). |
| `DEX_CONFIG`    | Explicit path to `config.json`.                          |
| `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_CACHE_HOME` | XDG base dirs for config/data/cache. |
| `HOME`               | Fallback when XDG vars are unset.                        |

## Project structure

```
dex/
├── .dex/
│   └── skills/           # (optional) project-level agent skills
├── target/
├── Cargo.lock
├── Cargo.toml
├── README.md
├── config.sample.json
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
    ├── tools/            # builtin tools: read, bash, write, edit, grep, find
    ├── skills.rs         # skill discovery
    └── ui/               # ratatui TUI (local event loop + remote client UI)
```

## How it works

A turn runs in `src/agent/loop.rs` (`process_turn`): it repeatedly calls the
model with tools enabled, executes any requested tool calls in parallel, feeds
results back, and compacts history once the message count or estimated token
budget is exceeded. A wrap-up nudge is injected when few tool-call iterations
remain. Progress is reported through a `Console` (streamed lines + approval
requests).

In client–server mode the daemon runs `process_turn` on a blocking thread and
translates console output into `StreamEvent`s over SSE
(`src/daemon/server.rs`). The TUI (`src/ui/remote.rs`) consumes those events
from a worker thread and renders them live; approvals and cancellation are
round-tripped over `POST .../approve` and `POST .../cancel`. The local TUI
(`src/ui/event.rs`) runs the same loop in-process with direct channels.

## License

See the repository for license information.
