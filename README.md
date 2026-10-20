# Steery drivers

A driver connects [Steery](https://steery.dev) to one harness: an agent product a person
runs conversations in. The Steery runtime starts a driver as a child process and speaks the
program protocol to it, JSON-RPC 2.0 over standard input and output (`PROTOCOL.md` in
[steery-dev/sdk](https://github.com/steery-dev/sdk)); the driver reads the harness's conversations, plans what setting it up
writes, and says how to start, read and resume a turn. It decides nothing and writes nothing
of its own. The runtime itself names no harness: everything it knows of one, a driver says.

Each driver is its own program, `steery-driver-<id>`, built on the `steery-sdk` crate, and
answers version 3 of the program protocol. Steery adds the drivers below the way it adds any
program: from this repository's releases, each a prebuilt executable per target
(`steery programs add https://github.com/steery-dev/drivers`, which Steery's installer
runs). Everything a person sees of a driver, its label, its short name, its tint and its
mark, is what its `initialize` says.

| Driver | Harness | Crate | Levels |
|---|---|---|---|
| `omp` | omp | [`pi-family`](pi-family/) | observe, drive, live (Steery's plugin door) |
| `pi` | pi | [`pi-family`](pi-family/) | observe, drive, live (Steery's plugin door) |
| `claude-code` | Claude Code | [`claude-code`](claude-code/) | observe, drive, live (Claude Code's inbox) |
| `codex` | Codex | [`codex`](codex/) | observe, drive, live (Codex's own doors: `picks`) |

Each crate is a Cargo workspace of its own, with its own `Cargo.lock`, so it builds alone.
What the drivers share is the [`common`](common/) crate, `steery-driver-common`, each
driver naming it by path (`../common`): finding the harness's executable and asking its
release under a bounded, killed probe (`program`); a transcript's lines read within
bounds (`lines`); a file only appended to, read on from where the last read left it
(`follow`); and a transcript that is a tree of records, kept between reads and read along
its path in batches, records off the path within the same bound (`tree`). Each driver
keeps only how its harness's records name their parent and what they say.

## Finding and installing the harness

A driver finds its harness's executable on `PATH`, then in the folder the harness's own
installer writes it to, which a shell that has not read its profile since may not have on
`PATH`; `setup/plan`'s `found.binary` is the one found, and every later start runs it.
Where the harness is not found, `setup/plan` answers `install`: the vendor's own installer,
which `steery setup` shows whole and runs only once the person agreed (PROTOCOL.md §4.1).

| Driver | Installer folder | `install` (offered where its shell and `curl` are on `PATH`) |
|---|---|---|
| `omp` | `PI_INSTALL_DIR`, else `~/.local/bin` | `sh -c "curl -fsSL https://omp.sh/install \| sh -s -- --binary"` |
| `pi` | its agent folder's `bin` (`~/.pi/agent/bin`) | `sh -c "curl -fsSL https://pi.dev/install.sh \| sh"` (offered only where npm is here and `node --version` names 22.19 or newer within 5 s) |
| `claude-code` | `~/.local/bin` | `bash -c "curl -fsSL https://claude.ai/install.sh \| bash"` |
| `codex` | `CODEX_INSTALL_DIR`, else `~/.local/bin` | `sh -c "curl -fsSL https://chatgpt.com/codex/install.sh \| CODEX_NON_INTERACTIVE=1 sh"` |

## Building

The SDK and the protocol live in [steery-dev/sdk](https://github.com/steery-dev/sdk).
Every crate names the SDK by git, at a tag,
`steery-sdk = { git = "https://github.com/steery-dev/sdk", tag = "v3.8.0" }`, and pins its
toolchain in `rust-toolchain.toml`:

```sh
git clone https://github.com/steery-dev/drivers
cd drivers/pi-family && cargo build
cargo test
```

Every build goes through [Kache](https://github.com/kunobi-ninja/kache), the compiler cache
`.cargo/config.toml` names. Install it once per machine:
`brew install kunobi-ninja/kunobi/kache && kache daemon install` on a Mac, its
[APT package](https://kunobi.ninja/docs/kache/getting-started/installation) on Linux.

`./release.sh <tag>` builds every driver with `--release --locked` for each target Steery
runs on, `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu` and
`aarch64-unknown-linux-gnu` (the Linux builds linking glibc 2.30 at most), with
`cargo zigbuild`, into `dist/<target>/`. It then packs them into `dist/release/` with
`steery programs pack --release <tag> --out dist/release <target>=dist/<target>…`: one
archive per driver and target and the manifest, `steery-programs.json` (PROTOCOL.md §7.1).
With `--publish` it makes the GitHub release `<tag>` of those files (`gh release create`);
with `--no-pack` it stops after building. It needs `cargo-zigbuild`, `zig`, the four targets
(`rustup target add`), and `steery` on `PATH` or named by `STEERY`.

## Checking a driver

```sh
steery programs test target/debug/steery-driver-omp
steery programs test target/debug/steery-driver-pi
```

`steery programs test [--setting NAME[=VALUE]]… <command> [args…]` starts the driver and
checks what it says it is, the methods that read and change nothing, an id sent twice, a
method it does not have, and its shutdown, one line per check; it exits 0 only when every
one passes. A read answered with a refusal word its method lists is an answer.
`--setting NAME=VALUE` passes a setting to `initialize`, and `--setting NAME` takes its value
from the environment, keeping a secret off the command line; a method that needs a required
setting not given is skipped. `models` asks the harness itself, so the harness must be on
`PATH` for that check to pass. Each crate's own `cargo test` covers what it reads and writes.

## Writing a driver

A driver for a harness Steery does not ship is a program like these. `pi-family/` is the
worked example; copy it and keep its shape (the layout is below, under omp and pi).

- **Its crate** depends on `steery-sdk` by git as above, and on `steery-driver-common` by
  path where it keeps a tree, follows a file or reads a receipt (in this repository
  `../common`; copied elsewhere, beside it), with `tokio` for its runtime and `serde_json`
  for the parameters, and declares an empty `[workspace]` so it builds alone.
  Its binary is named `steery-driver-<id>`; `src/bin/omp.rs` is one line,
  `steery_driver_pi_family::serve(Harness::Omp)`, which calls
  `steery_sdk::serve(|_notifier| Driver::new(harness))`. The SDK reads requests, answers
  each one, and gives the notifier for what the driver says unasked.
- **`initialize`** (`Program::initialize` in `src/lib.rs`) refuses a runtime that does not
  offer `steery_sdk::PROTOCOL`, keeps the `home` and `config` it is given, and answers
  `wire::Initialized`: `program` (`id`, `kind: Harness`, `release`, `label`, `short`, and
  optionally `tint` and `mark`, what a person sees of it),
  `harness` (`HarnessInfo` from `src/harness.rs`: its levels, the harness's minimum release,
  how it signs in, its facts, its hooks and its door; PROTOCOL.md §3.1), no `machine`, and
  the settings it needs (none here; §3.4).
- **The methods** (`Program::call`) are one match from each `steery_sdk::method` name to the
  module that answers it, its parameters read with `steery_sdk::params` and its answer
  given with `steery_sdk::result`: observe (`setup/plan`, `sessions/list`,
  `sessions/locate`, `sessions/read`, `hook/read`; §4.1), drive (`held`, `idle`, `models`,
  `start`, `turn/read`, `receipt/*`, `reader`, `terminal`; §4.2), and any other answers
  `Error::Unknown`. A refusal is `Error::refused` with one of the protocol's words
  (`steery_sdk::refusal`).
- **Live** (§4.3) is a door. A harness with its own (`picks`, `keeps`) answers `say`,
  `pick`, `stop` and `recap` in its driver, as `claude-code` and `codex` do. omp and pi have
  none, so their door is `plugin`: Steery's plugin, which this repository keeps in
  `pi-family/plugin/` and `setup/plan` installs, speaks the plugin door, version 1 (§4.4),
  to the runtime itself, and the driver has no live methods. The runtime uses the live
  methods only on conversations Steery started; a conversation a person started is never
  steered.

## Adding a driver

A driver is added as every program is, Steery's own included (PROTOCOL.md §7.2):

```sh
steery programs add https://github.com/steery-dev/drivers
steery programs add https://github.com/me/my-harness@v0.1.0 --setting MY_HARNESS_TOKEN
```

`<source>` is a GitHub repository (its latest release, followed by `steery programs
update`, or `@<tag>` for that one), a release's manifest, `steery-programs.json`, by URL or
path, one program's archive, or a built `steery-driver-<id>` itself. Publish your own
driver the way this repository does: build it for each target, pack it with
`steery programs pack`, and make the release of those files. Nothing is built on the
computer it is added to.

Each program's build for this computer is fetched, checked against its sha256, copied into
Steery's programs folder, started and checked as `steery programs test` checks it, with the
settings given. A driver's settings (PROTOCOL.md §3.4), an API key or the account it acts
as, are given here once: `--setting NAME=VALUE`, or `--setting NAME` to take the value from
the environment; a secret one not given is typed at the terminal, unechoed. Steery prints
what it will add and asks once at the terminal; `--yes` is that answer anywhere else. The
runtime sends the settings in each `initialize` it sends the driver: it runs as a service
(launchd, systemd) whose environment holds none of the person's shell variables, so a
driver reads nothing it needs from its environment.

`steery programs remove <id>` removes it.

## omp and pi: `pi-family/`

omp is a fork of pi. Both write the same transcript, load the same plugin, read the same
`PI_*` variables for their folders and run kept in their RPC mode, so one
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
- **Drive** (`held`, `idle`, `models`, `start`, `turn/read`, `receipt/*`, `reader`,
  `terminal`).
  - `held`: no registry, so the runtime looks for processes. In pi it also looks for a
    stray process, since a pi without the plugin holds no file open.
  - `models`: omp's comes from `omp models --json`, with its default from `config.yml`.
    pi's comes from `pi --list-models` and its RPC mode's `get_available_models`.
  - `start`: a turn is a kept process, `--mode rpc` (omp's with `--no-ui`) with its
    standard input held: it stays up between turns and takes every turn's words, the first
    included, through the plugin door. omp continues by id (`--resume`) and checks a model
    against its catalog first. pi continues by transcript (`--session <file>`) and passes a
    model to the plugin as well (`--steery-model`), which ends the process with a refusal
    where pi does not run it exactly.
  - `turn/read`: the harness's own refusals on standard error; its standard output is
    discarded, and its door says what it does.
  - `receipt/mark`, `receipt/find`: the person's message carrying the delivery marker, in
    the transcript past a mark, or in all of it where the harness rewrote the file since
    (some omp compactions do); words the door hands over mid-turn go in at the next tool.
  - `reader`: a one-shot `--print` reader with everything closed that a flag closes.
  - `terminal`: the command line that resumes a conversation in a terminal.

The live methods (`say`, `pick`, `stop`, `recap`, `compact`) are the plugin door's (PROTOCOL.md
§4.4), which the runtime speaks itself, so the drivers have none. The runtime uses them
only on conversations Steery started.

### Layout: a template for a harness Steery does not ship

To connect another harness, copy `pi-family/` and keep its shape:

| File | What it holds |
|---|---|
| `src/bin/<id>.rs` | The binary: `serve(Harness::…)`. |
| `src/lib.rs` | `initialize`, and one match from each method to the module that answers it. Methods the harness cannot do answer `Error::Unknown`. |
| `src/harness.rs` | Everything about the harness as Steery reads it: id, `HarnessInfo` (levels, minimum, identity, facts, hooks), its folders, its executable. Every other module asks here and never names a harness itself. |
| `src/transcripts.rs` | Where the harness files conversations; `sessions/list`, `locate`, `read`, and the cursor's `mark`: the rest of the cursor and the batched read are `steery_driver_common::tree`'s. |
| `src/tree.rs` | The transcript's format: here, how pi's entries name their parent, as a `steery_driver_common::tree::Shape`. |
| `src/events.rs` | One record → the protocol's events. |
| `src/calls.rs` | A tool call → `Call`: its group, MCP server, Steery's own tool, the questions it asks. |
| `src/hooks.rs` | `hook/read`: a hook's document → `Moment`. |
| `src/models.rs` | `models`: what the harness lists and the files whose change changes it. |
| `src/drive.rs` | `held`, `idle`, `start`, `turn/read`, `receipt/*`, `reader`, `terminal`. |
| `src/setup.rs` | `setup/plan`: the entries, each with what it carries. |
| `plugin/` | Steery's plugin for harnesses whose door is `plugin`: the extension and its loader, embedded in the binary at build time. |

A harness with its own door (`picks`, `keeps`) has no `plugin/` and answers `say`, `pick`
and `recap` instead. One whose moments are hook commands answers `hook/answer` too.

### The plugin

`pi-family/plugin/extension.ts` speaks the plugin door, version 1. It draws nothing in the
harness and takes no answer there. `loader.ts` is the file the harness loads: it runs the
copy of the extension the runtime's config folder names, and moves every session onto a
newer copy once one is named. Typecheck them against pi's published types:

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
- **Moments** (`hook/read`, `hook/answer`). A hook's document becomes a Moment, with the
  state Claude Code's documents carry: the model (`SessionStart`'s `model`,
  `PostModelSwitch`'s `to_model`), the effort (`effort.level`) and, at `StopFailure`, the
  failure (`error`, its words in `last_assistant_message`). They carry no time, and
  `PreCompact`/`PostCompact` carry no compaction's id or tokens, so compactions stay the
  transcript's. Steery's answer becomes what Claude Code reads:
  `hookSpecificOutput.additionalContext`, a `Stop` that keeps the turn going (`decision:
  block`), a `PreToolUse` or `PermissionRequest` decision, and the conversation's lead
  written to `CLAUDE_ENV_FILE` (`append`).
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
    Code's own refusals (`untrusted`, `not-found`, `harness-busy`). The launch gives that
    session its own `Stop` and `StopFailure` hooks (`--settings`), which run the binary's
    `heard` mode into a file the follower reads: whether a stop the listing shows is this
    turn's, and Claude Code's words for an API error. No transcript is read.
  - `receipt/mark`, `receipt/find`: the person's input carrying the delivery marker, in
    the transcript past a mark.
  - `models`: the model in `settings.json`, and Claude Code's effort levels for any other.
  - `reader`: `claude --print` with no tools, no MCP server, `--safe-mode` and no session
    kept.
  - `terminal`: `claude --resume <id>`, or `claude attach <job>` while a background session
    holds it, and the Claude app's link for a conversation it started.
- **Live** (`say`), for a conversation Steery started that a Claude Code session holds.
  Words go into the inbox socket the registered session opens, after its peer token; the socket says nothing back, so they are unconfirmed and the runtime finds
  them with `receipt/find`. The session keeps its own model, so `pick` is unsupported, and
  there is no `recap`.

```sh
cd claude-code && cargo build
steery programs test target/debug/steery-driver-claude-code
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
- **Live** (`say`, `pick`, and `session/changed`), for a conversation Steery started. Words go to the window that owns the
  thread, as typed in its composer, else into the running app server's queue, whose
  acknowledgement is not delivery (`took: false`). A pick is checked against the owner's
  own model list and set as the thread's settings. The windows' own state of each thread
  the runtime asks about is followed and said as `session/changed`. Codex has no recap
  short of a turn, so `recap` is unknown.

Build and check it, with Codex on `PATH`:

```sh
cd codex && cargo build
steery programs test target/debug/steery-driver-codex
cargo test
```
## License

MIT, see [LICENSE](LICENSE).
