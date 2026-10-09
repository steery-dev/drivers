# Steery drivers

A driver connects [Steery](https://steery.dev) to one harness: an agent product a person
runs conversations in. The Steery runtime starts a driver as a child process and speaks the
program protocol to it, JSON-RPC 2.0 over standard input and output (`specs/PROTOCOL.md` in
Steery's repository); the driver reads the harness's conversations, plans what setting it up
writes, and says how to start, read and resume a turn. It decides nothing and writes nothing
of its own. The runtime itself names no harness: everything it knows of one, a driver says.

Each driver is its own program, `steery-driver-<id>`, built on the `steery-sdk` crate, and
answers version 1 of the program protocol. The official ones below ship in Steery's release
archive, beside `steery`, each built from this repository at the commit `programs.lock` in
Steery's repository names, and update with it.

| Driver | Harness | Crate | Levels |
|---|---|---|---|
| `omp` | omp | [`pi-family`](pi-family/) | observe, drive, live (Steery's plugin door) |
| `pi` | pi | [`pi-family`](pi-family/) | observe, drive, live (Steery's plugin door) |
| `claude-code` | Claude Code | [`claude-code`](claude-code/) | observe, drive, live (Claude Code's inbox) |
| `codex` | Codex | [`codex`](codex/) | observe, drive, live (Codex's own doors: `picks`) |

Each crate is a Cargo workspace of its own, with its own `Cargo.lock`, so it builds alone.

## Finding and installing the harness

A driver finds its harness's executable on `PATH`, then in the folder the harness's own
installer writes it to, which a shell that has not read its profile since may not have on
`PATH`; `setup/plan`'s `found.binary` is the one found, and every later start runs it.
Where the harness is not found, `setup/plan` answers `install`: the vendor's own installer,
which `steery setup` shows whole and runs only once the person agreed (PROTOCOL.md §4.1).

| Driver | Installer folder | `install` (offered where its shell and `curl` are on `PATH`) |
|---|---|---|
| `omp` | `PI_INSTALL_DIR`, else `~/.local/bin` | `sh -c "curl -fsSL https://omp.sh/install \| sh -s -- --binary"` |
| `pi` | its agent folder's `bin` (`~/.pi/agent/bin`) | `sh -c "curl -fsSL https://pi.dev/install.sh \| sh"` (needs Node.js and npm) |
| `claude-code` | `~/.local/bin` | `bash -c "curl -fsSL https://claude.ai/install.sh \| bash"` |
| `codex` | `CODEX_INSTALL_DIR`, else `~/.local/bin` | `sh -c "curl -fsSL https://chatgpt.com/codex/install.sh \| CODEX_NON_INTERACTIVE=1 sh"` |

## Building

The SDK and the protocol live in Steery's own repository. Every crate names the SDK by path,
`steery-sdk = { path = "../../server/crates/steery-sdk" }`, so check Steery's repository out
beside this one as `server`:

```sh
git clone https://github.com/steery-dev/server
git clone https://github.com/steery-dev/drivers
cd drivers/pi-family && cargo build
cargo test
```

A release builds each official driver the same way, from its own crate, with `--locked` and
the `ship` profile of Steery's `Cargo.toml` (`deploy/compile-cli.sh` in Steery's
repository):

```sh
cargo build --profile ship --locked --config <ship profile> \
  --manifest-path pi-family/Cargo.toml --bin steery-driver-omp
```

## Checking a driver

```sh
steery driver test target/debug/steery-driver-omp
steery driver test target/debug/steery-driver-pi
```

`steery driver test <command> [args…] [--setting NAME[=VALUE]]` starts the driver and checks
what it says it is, the methods that read and change nothing, an id sent twice, a method it
does not have, and its shutdown, one line per check; it exits 0 only when every one passes.
A read answered with a refusal word its method lists is an answer. `--setting NAME=VALUE`
passes a setting to `initialize`, and `--setting NAME` takes its value from the environment,
keeping a secret off the command line; a method that needs a required setting not given is
skipped. `models` asks the harness itself, so the harness must be on `PATH` for that check
to pass. Each crate's own `cargo test` covers what it reads and writes.

## Writing a driver

A driver for a harness Steery does not ship is a program like these. `pi-family/` is the
worked example; copy it and keep its shape (the layout is below, under omp and pi).

- **Its crate** depends on `steery-sdk` by path as above, with `tokio` for its runtime and
  `serde_json` for the parameters, and declares an empty `[workspace]` so it builds alone.
  Its binary is named `steery-driver-<id>`; `src/bin/omp.rs` is one line,
  `steery_driver_pi_family::serve(Harness::Omp)`, which calls
  `steery_sdk::serve(|_notifier| Driver::new(harness))`. The SDK reads requests, answers
  each one, and gives the notifier for what the driver says unasked.
- **`initialize`** (`Program::initialize` in `src/lib.rs`) refuses a runtime that does not
  offer `steery_sdk::PROTOCOL`, keeps the `home` and `config` it is given, and answers
  `wire::Initialized`: `program` (`id`, `kind: Harness`, `release`, `label`, `short`),
  `harness` (`HarnessInfo` from `src/harness.rs`: its levels, the harness's minimum release,
  how it signs in, its facts, its hooks and its door; PROTOCOL.md §3.1), no `machine`, and
  the settings it needs (none here; §3.4).
- **The methods** (`Program::call`) are one match from each `steery_sdk::method` name to the
  module that answers it, its parameters read with `steery_sdk::params` and its answer
  given with `steery_sdk::result`: observe (`setup/plan`, `sessions/list`,
  `sessions/locate`, `sessions/read`, `hook/read`; §4.1), drive (`held`, `idle`, `models`,
  `start`, `turn/read`, `reader`, `terminal`; §4.2), and any other answers `Error::Unknown`.
  A refusal is `Error::refused` with one of the protocol's words (`steery_sdk::refusal`).
- **Live** (§4.3) is a door. A harness with its own (`picks`, `keeps`) answers `say`,
  `pick`, `stop` and `recap` in its driver, as `claude-code` and `codex` do. omp and pi have
  none, so their door is `plugin`: Steery's plugin, which this repository keeps in
  `pi-family/plugin/` and `setup/plan` installs, speaks the plugin door, version 1 (§4.4),
  to the runtime itself, and the driver has no live methods.

## Adding a driver to an install

An official driver ships in Steery's archive, beside `steery`. Any other is added with
`steery driver add`:

```sh
steery driver add <source> [--crate <dir>] [--wait-secs <n>]
steery driver add https://github.com/steery-dev/drivers#<rev> --crate pi-family
```

`<source>` is an executable's path; a folder with a `Cargo.toml`, built there with
`cargo build --release --locked` (refused without a `Cargo.lock` at its workspace root, which
every crate here has); or a git repository's URL (one with `://`, or `git@host:`), with an
optional `#<rev>` (a branch, tag or commit), cloned to `<config>/programs/src/<id>` and built
the same way. `--crate` names the crate's folder inside the repository. The official ids,
`omp`, `pi`, `claude-code` and `codex`, are refused, so a fork of a crate here must answer `initialize` with an id of its
own.

What runs is a copy, `<config>/programs/bin/<id>-<the first 12 hex of its digest>`. Before
anything is asked, the copy is started and checked as `steery driver test` checks it: an
official id, a kind other than a driver's, an `initialize` that fails or speaks another
protocol, or any failed check refuses it, and nothing is written. Then one card on the
person's phone, "Add the driver <label>", names it, its source, release, digest, what it
provides and the settings it declares. Only a verified Go writes its record,
`<config>/drivers/<id>.json`:

```json
{"id": "my-harness", "kind": "harness",
 "command": "<config>/programs/bin/my-harness-<12 hex>", "args": [],
 "source": "https://github.com/me/my-harness#<commit>",
 "release": "0.1.0", "digest": "sha256:<64 lowercase hex>"}
```

`<config>` is this computer's Steery folder, `$XDG_CONFIG_HOME/steery`, else
`~/.config/steery`; every machine this computer runs reads the records there, a sandbox
machine's runtime (`steery run --machine`) too. `digest` is `sha256:` and the SHA-256 of the
file `command` names, in lowercase hex. `source` is the words that add it again: the
absolute path, the absolute folder, or `<url>#<commit> [--crate <dir>]`, pinned to the
commit built. The command exits 0 once added, 1 on No or an answer that does not verify,
and 2 with no answer within `--wait-secs` (by default and at most 3600).

The runtime checks the digest before every start: a driver whose copy changed on disk is
not started, and reads Missing with "add it again with `steery driver add <source>`".
`steery driver remove <id>` deletes the record, its copy and its clone; an official id with
no record is refused.

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

## Codex: `codex/`

One binary, `steery-driver-codex`, with no settings. It speaks to Codex only through what
Codex publishes: its rollout files, `hooks.json` and `config.toml`, `codex app-server` (its
JSON-RPC over standard input and output), `codex app-server proxy` (the bytes of a running
app server's control socket), the Codex app's router `<CODEX_HOME>/ipc/ipc.sock`, `codex exec`,
`codex mcp list` and `codex features list`. Codex's home is `CODEX_HOME`, else `~/.codex`.

- **Conversations** (`sessions/list`, `sessions/locate`, `sessions/read`). Rollouts are read
  from `sessions/<yyyy>/<mm>/<dd>/rollout-<started-at>-<thread>.jsonl`. A thread continued in
  more than one rollout is one conversation, its newest rollout the current one; two
  equally current ones name it ambiguously (`complete: false`). A sub-agent's rollout (its
  `session_id` names another thread) is listed under its lead, never as a conversation, and
  so is not a `codex exec` run. A fork's history (`history_base`) is read from its parent's
  rollout up to where the fork took it, then a `fork` event, then its own. Events are turns
  and how they ended (a failure classified by its status or Codex's own code), the
  person's words with the files and pictures they attached (Codex's own fragments left
  out), the agent's words and reasoning, calls and their results, the running token total
  with the context left, models and efforts, sub-agents started and how they ended, and
  rollbacks; a rollback that does not say how many turns went is refused.
- **Setup** (`setup/plan`). The MCP entry in `config.toml` (`mcp_servers.steery`, its
  `tool_timeout_sec` the runtime's bound), the skill in `skills/steery/SKILL.md`, and one
  hook group per moment in `hooks.json`, beside the person's own (`/hooks/<Event>/-`).
  Codex runs a hook only once its person trusted it, and keys that trust by the hook's
  place: where a Steery hook its person trusted moved because another tool put a group
  ahead of it, the plan writes the same trust again where it now stands (asked of Codex's
  own `hooks/list`, never computed here).
- **Hooks** (`hook/read`, `hook/answer`). Codex's documents become Moments; an answer
  becomes what Codex parses: `additionalContext` on `SessionStart` and `UserPromptSubmit`,
  `decision: "block"` at `Stop`, a `permissionDecision` on `PreToolUse`, and a `decision`
  with its `behavior` (and the standing permission chosen) on `PermissionRequest`.
- **Drive.**
  - `held`: a native owner where one takes the thread's words: the window of the Codex app
    that owns it, or a running app server of its home that has it loaded with nothing
    queued. Else the runtime looks, an app server holding it being beyond reach.
  - `idle`: the owner's own state, else where the rollout's newest turn stands, while
    nothing holds the rollout.
  - `start`: this binary in its turn mode, `steery-driver-codex turn --codex <codex>
    <continue|fork|new> [<thread>] [--worktree] [--model m] [--thinking t] --inbox <socket>`.
    It opens the thread (`thread/resume`, `thread/fork`, `thread/start`), starts the turn,
    writes every message of the app server to standard output, takes more words on the
    inbox (`turn/steer`, or `turn/start` once the turn ended) and ends with the last turn.
    A worktree is made where Codex makes its own, `<CODEX_HOME>/worktrees/<4 hex>/<repo>`.
  - `turn/read`: the thread named, the words taken (by their delivery marker), how each turn
    completed, and the app server's refusals (`harness-busy` beside a live writer).
  - `stop`: the owner interrupts its own turn and it is seen to end.
  - `receipt/mark`, `receipt/find`: the thread's turns before the words, and the one turn
    after them carrying the delivery marker.
  - `models`: the app server's `model/list`, the default first.
  - `reader`: `codex exec --json --ephemeral` with every MCP server turned off, every tool
    and feature it can do without taken away, and a read-only sandbox; `reader/read` takes
    its last agent message.
  - `terminal`: `codex resume <thread>`, and the app's `codex://threads/<thread>`.
- **Live** (`say`, `pick`, and `session/changed`). Words go to the window that owns the
  thread, as typed in its composer, else into the running app server's queue, whose
  acknowledgement is not delivery (`took: false`). A pick is checked against the owner's
  own model list and set as the thread's settings. The windows' own state of each thread
  the runtime asks about is followed and said as `session/changed`. Codex has no recap
  short of a turn, so `recap` is unknown.

Build and check it, with Codex on `PATH`:

```sh
cd codex && cargo build
steery driver test target/debug/steery-driver-codex
cargo test
```
## License

MIT, see [LICENSE](LICENSE).
