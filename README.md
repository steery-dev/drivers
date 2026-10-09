# Steery drivers

A driver connects [Steery](https://steery.dev) to one harness: an agent product a person
runs conversations in. The Steery runtime starts a driver as a child process and speaks the
program protocol to it, JSON-RPC 2.0 over standard input and output; the driver reads the
harness's conversations, plans what setting it up writes, and says how to start, read and
resume a turn. It decides nothing and writes nothing of its own.

Each driver is its own program, built on the `steery-sdk` crate, and answers version 1 of
the program protocol (`specs/PROTOCOL.md` in Steery's repository).

| Driver | Harness | Crate | Levels |
|---|---|---|---|
| `omp` | omp | [`pi-family`](pi-family/) | observe, drive, live (Steery's plugin door) |
| `pi` | pi | [`pi-family`](pi-family/) | observe, drive, live (Steery's plugin door) |
| `claude-code` | Claude Code | [`claude-code`](claude-code/) | observe, drive, live (Claude Code's inbox) |

`codex` follows.

## Building

The SDK and the protocol live in Steery's own repository until they are published. Check it
out beside this one, as `server`:

```sh
git clone https://github.com/steery-dev/server
git clone https://github.com/steery-dev/drivers
cd drivers/pi-family && cargo build
steery driver test target/debug/steery-driver-omp
steery driver test target/debug/steery-driver-pi
cargo test
```

`steery driver test` starts the driver and checks what it says it is, the methods that read
and change nothing, an id sent twice, a method it does not have, and its shutdown. `models`
asks the harness itself, so the harness must be on `PATH` for that check to pass.

## omp and pi: `pi-family/`

omp is a fork of pi. Both write the same transcript, load the same plugin, read the same
`PI_*` variables for their folders and speak the same JSON lines in a print run, so one
crate holds both drivers: a library, and two binaries, `steery-driver-omp` and
`steery-driver-pi`, each one line that serves the library for its harness. Neither driver
has settings.

What each driver does:

- **Conversations** (`sessions/list`, `sessions/locate`, `sessions/read`). They are read
  from the transcripts under the agent folder (`~/.omp/agent`, `~/.pi/agent`, or
  `PI_CODING_AGENT_DIR`; omp also `PI_CONFIG_DIR` and `OMP_PROFILE`), in
  `sessions/<folder>/<started-at>_<id>.jsonl`. They are also read from the folder
  `PI_CODING_AGENT_SESSION_DIR` names, from pi's `sessionDir` setting, and from the folders
  of the transcripts the harness's moments named. A transcript is a tree: each entry names
  its parent. The conversation is the path from the last entry back to a root, and only
  that path is read. Where the conversation went back to an earlier entry and on from
  there, the next read starts again from the beginning (`reset`). Events are turns and how
  they ended, words, calls grouped as Steery groups them, tokens per response, models (in
  pi with their thinking level), forks and omp's sub-agents. A failed model call ends its
  turn only once the harness went no further. What a branch left behind spent is still
  given, as each of its responses' `tokens`, once.
- **Setup** (`setup/plan`). It plans the skill, Steery's plugin and, in omp, the MCP entry
  in `mcp.json`. The plugin is a loader in `extensions/steery.ts` plus the copy of the
  extension it runs, kept in the runtime's config folder. In pi the plugin carries the MCP
  entry and registers it for each conversation, with headers naming that conversation.
  Where omp and pi are both installed and share one folder, neither is set up.
- **Moments** (`hook/read`). The plugin forwards each moment the harness gives it over the
  door, and the driver reads it as a Moment.
- **Drive** (`held`, `idle`, `models`, `start`, `turn/read`, `reader`, `terminal`).
  - `held`: no registry, so the runtime looks for processes. In pi it also looks for a
    stray process, since a pi without the plugin holds no file open.
  - `models`: omp's comes from `omp models --json`, with its default from `config.yml`.
    pi's comes from `pi --list-models` and its RPC mode's `get_available_models`.
  - `start`: a turn is `-p --mode json`. omp continues by id (`--resume`) and checks a
    model against its catalog first. pi continues by transcript (`--session <file>`) and
    passes a model to the plugin as well (`--steery-model`), which refuses one pi does not
    run exactly.
  - `turn/read`: reads the run's JSON lines and the harness's own refusals.
  - `reader`: a one-shot `--print` reader with everything closed that a flag closes.
  - `terminal`: the command line that resumes a conversation in a terminal.

The live methods (`say`, `pick`, `stop`, `recap`, `receipt/*`) are the plugin door's
(PROTOCOL.md §4.4), which the runtime speaks itself, so the drivers have none.

### Layout: a template for a harness Steery does not ship

To connect another harness, copy `pi-family/` and keep its shape:

| File | What it holds |
|---|---|
| `src/bin/<id>.rs` | The binary: `serve(Harness::…)`. |
| `src/lib.rs` | `initialize`, and one match from each method to the module that answers it. Methods the harness cannot do answer `Error::Unknown`. |
| `src/harness.rs` | Everything about the harness as Steery reads it: id, `HarnessInfo` (levels, minimum, identity, facts, hooks), its folders, its executable. Every other module asks here and never names a harness itself. |
| `src/transcripts.rs` | Where the harness files conversations; `sessions/list`, `locate`, `read`, and the cursor. |
| `src/tree.rs` | The transcript's format: here, a tree read along its active path. |
| `src/events.rs` | One record → the protocol's events. |
| `src/calls.rs` | A tool call → `Call`: its group, MCP server, Steery's own tool, the questions it asks. |
| `src/hooks.rs` | `hook/read`: a hook's document → `Moment`. |
| `src/models.rs` | `models`: what the harness lists and the files whose change changes it. |
| `src/drive.rs` | `held`, `idle`, `start`, `turn/read`, `reader`, `terminal`. |
| `src/setup.rs` | `setup/plan`: the entries, each with what it carries. |
| `plugin/` | Steery's plugin for harnesses whose door is `plugin`: the extension, its loader and the form rule, embedded in the binary at build time. |

A harness with its own door (`picks`, `keeps`) has no `plugin/` and answers `say`, `pick`
and `recap` instead. One whose moments are hook commands answers `hook/answer` too.

### The plugin

`pi-family/plugin/extension.ts` speaks the plugin door, version 1. `loader.ts` is the file
the harness loads: it runs the copy of the extension the runtime's config folder names,
and moves every session onto a newer copy once one is named. `form.js` is the rule every
surface draws a request by. Typecheck them against pi's published types:

```sh
cd pi-family/plugin && npm i && npm run check
```

## Claude Code: `claude-code/`

One crate, one binary, `steery-driver-claude-code`. It has no settings. Claude Code keeps
everything under its configuration root, `CLAUDE_CONFIG_DIR` or `~/.claude`.

- **Conversations** (`sessions/list`, `sessions/locate`, `sessions/read`). They are read
  from `projects/<folder>/<id>.jsonl` under the root, and from the `projects` folder of any
  transcript the harness's moments named. A conversation resumed goes on in its own file.
  A sub-agent's transcript is filed under its conversation's,
  `<id>/subagents/agent-<agent>.jsonl`, with the `.meta.json` beside it naming its type,
  task and the call that started it. A run a program started (`entrypoint: sdk-cli`) is
  no conversation until a person carries it on; one the Claude app started
  (`entrypoint: claude-desktop`) is `app`. Events are turns and how they ended (an API
  error with its status and code), words, calls grouped as Steery groups them, tokens per
  response, models and sub-agents.
- **Setup** (`setup/plan`). Steery's MCP entry in `.claude.json` (beside the root where
  it is `~/.claude`), the skill in `skills/steery/SKILL.md`, and one hook per event in
  `settings.json`, at `/hooks/<event>/-`: one group beside the person's own that runs
  `steery hook claude-code <event>`. `found.release` is what `claude --version` says.
- **Moments** (`hook/read`, `hook/answer`). A hook's document becomes a Moment; Steery's
  answer becomes what Claude Code reads: `hookSpecificOutput.additionalContext`, a
  `Stop` that keeps the turn going (`decision: block`), a `PreToolUse` or
  `PermissionRequest` decision, and the conversation's lead written to `CLAUDE_ENV_FILE`
  (`append`).
- **Drive** (`held`, `idle`, `stop`, `receipt/*`, `models`, `start`, `turn/read`,
  `reader`, `terminal`).
  - `held`: Claude Code's registry, `sessions/<pid>.json`, names the live session in a
    conversation. A background session is its owner's to stop, by `claude stop`, through
    an owner token; a terminal is a process the runtime stops. Without a registered
    session, the runtime looks for processes holding the transcript.
  - `idle`: what `claude agents --json --all` lists, with the registry.
  - `start`: a turn is a background session (`claude --bg`, `--resume`, `--fork-session`,
    `-w`, `--model`, `--effort`). The plan runs this driver's own binary in its turn mode
    (`steery-driver-claude-code turn …`), which starts the session and follows it through
    `claude agents` to where its turn stopped; `turn/read` reads those lines and Claude
    Code's own refusals (`untrusted`, `not-found`, `harness-busy`).
  - `receipt/mark`, `receipt/find`: the person's input carrying the delivery marker, in
    the transcript past a mark.
  - `models`: the model in `settings.json`, and Claude Code's effort levels for any other.
  - `reader`: `claude --print` with no tools, no MCP server, `--safe-mode` and no session
    kept.
  - `terminal`: `claude --resume <id>`, or `claude attach <job>` while a background session
    holds it, and the Claude app's link for a conversation it started.
- **Live** (`say`). Words go into the inbox socket the registered session opens, after its
  peer token; the socket says nothing back, so they are unconfirmed and the runtime finds
  them with `receipt/find`. The session keeps its own model, so `pick` is unsupported, and
  there is no `recap`.

```sh
cd claude-code && cargo build
steery driver test target/debug/steery-driver-claude-code
cargo test
```

`cargo test` needs no Claude Code; the reader's test runs only where `claude` is on `PATH`.

## License

MIT, see [LICENSE](LICENSE).
