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
| `grok-bot` | Grok Bot | [`grok-bot`](grok-bot/) | observe, drive, live (native gateway) |

`codex` and `claude-code` follow.

Grok Bot uses `grokbot-gateway.mjs` beside its binary, or the `cli` setting.
The wrapper reads the signed-in macOS app account through Keychain. Unlock Keychain
before a driver call. Other platforms require `CURSOR_ACCESS_TOKEN` and
`SAND_CLIENT_VERSION`. Credentials stay in process memory.

The driver reads gateway conversations, continues or creates bots, and proves accepted
input from its nonce. It does not select models, fork history, steer a running turn,
or expose native permission hooks. `held` reports `nobody` when both gateway running
flags are false, otherwise a holder whose stop is `beyond`. Stop remote turns in Grok Bot.

Gateway records are not host files: `transcripts` is empty and `harness.transcript`
is absent. `sessions/read` still reads gateway records. Carry is unavailable.

Connect Steery once in your own Grok Bot account. `setup/plan` observes `getMcpState`,
accepts a connected personal connector with the requested URL, and otherwise refuses
with that step in `notes`. It never changes account configuration. No local setup
entries apply because this harness does not read local skills or configuration.

No per-bot URL or header substitution was found in the installed app's connector
schema and resolver. Supply your own bot ID as `session.external_id` with
`session.harness=grok-bot`. If the bot cannot see its own ID, it must not describe
a session or guess from the roster.

Build against SDK `1.0.0` from server commit `54a34e8ed`, checked out beside this
repository as `steery-grokbot-cutover`: `cargo build --manifest-path grok-bot/Cargo.toml`.
The cutover runtime passed all 9 driver conformance checks and an isolated,
digest-pinned added-driver `Host::local` smoke through real gateway list/read calls.
Cold Keychain access exceeded the runtime's 30-second call bound. Supply
`CURSOR_ACCESS_TOKEN` and `SAND_CLIENT_VERSION` in the runtime environment to avoid
that access. Never put credentials in driver registration JSON.

Register the binary in `<config>/drivers/grok-bot.json` with `id`, `kind: "harness"`,
`command`, `args`, `source`, `release` and `digest: "sha256:<binary hash>"`.
Use file mode `0600`. Put `grokbot-gateway.mjs` beside the binary.
The phone installation card remains unavailable. The live machine report and
phone harness selection are not yet verified.


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

## License

MIT, see [LICENSE](LICENSE).
