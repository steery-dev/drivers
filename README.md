# Steery drivers

A driver connects [Steery](https://steery.dev) to one harness. A harness is an agent product
in which a person runs conversations. The Steery runtime starts a driver as a child process.
The runtime speaks the program protocol to the driver: JSON-RPC 2.0 over standard input and
output (`PROTOCOL.md` in [steery-dev/sdk](https://github.com/steery-dev/sdk)). The driver does
these things:

- reads the harness's conversations
- plans what the setup of the harness writes
- says how to start, read and resume a turn

The driver decides nothing and writes nothing of its own. The runtime itself names no harness.
A driver tells the runtime everything that the runtime knows about a harness.

Each driver is its own program, `steery-driver-<id>`, built on the `steery-sdk` crate. A driver
answers with the newest version of the program protocol that the runtime offers, of those in
`PROTOCOLS` (`common/src/program.rs`), which also says what each version adds. Every runtime
that Steery supports offers the oldest of them. The driver keeps the version that it picked.

Steery adds the drivers below in the same way as it adds any program: from this repository's
releases. Each release has a prebuilt executable per target. The command is
`steery programs add https://github.com/steery-dev/drivers`, and Steery's installer runs it.
Everything that a person sees of a driver comes from its `initialize` answer: its label, its
short name, its tint and its mark.

| Driver | Harness | Crate | Levels |
|---|---|---|---|
| `omp` | omp | [`pi-family`](pi-family/) | observe, drive, live (Steery's plugin door) |
| `pi` | pi | [`pi-family`](pi-family/) | observe, drive, live (Steery's plugin door) |
| `claude-code` | Claude Code | [`claude-code`](claude-code/) | observe, drive, live (Claude Code's inbox) |
| `codex` | Codex | [`codex`](codex/) | observe, drive, live (Codex's own doors: `picks`) |
| `grok` | Grok Build | [`acp-family`](acp-family/) | observe |
| `kilo` | Kilo | [`acp-family`](acp-family/) | observe |
| `gemini` | Gemini CLI | [`acp-family`](acp-family/) | observe |

Each crate is a Cargo workspace of its own, with its own `Cargo.lock`. Thus each crate builds
alone. The drivers share the [`common`](common/) crate, `steery-driver-common`. Each driver
names it by path (`../common`). The `common` crate has these modules:

- `program`: finds the harness's executable and asks its release, with a bounded probe that it kills past its bound.
- `lines`: reads a transcript's lines within bounds.
- `follow`: reads a file that its writer only appends to, from the point where the last read stopped.
- `tree`: keeps a transcript that is a tree of records between reads.
  It reads the transcript along its path in batches. Then it reads the records off the path,
  within the same bound.
- `login`: runs the harness's own sign-in on a terminal of the driver's (see Signing in).

Each driver keeps only two things: how its harness's records name their parent, and what the
records say.

## Finding and installing the harness

A driver looks for its harness's executable in two places, in this order:

1. On `PATH`.
2. In the folder where the harness's own installer writes it. A shell that did not read its
   profile after the install possibly does not have this folder on `PATH`.

`setup/plan` gives the executable it found as `found.binary`. Every later start runs this
executable. When the driver does not find the harness, `setup/plan` answers `install`. This is
the vendor's own installer. `steery setup` shows it whole, and runs it only after the person
agrees (PROTOCOL.md §4.1).

| Driver | Installer folder | `install` (offered where its shell and `curl` are on `PATH`) |
|---|---|---|
| `omp` | `PI_INSTALL_DIR`, else `~/.local/bin` | `sh -c "curl -fsSL https://omp.sh/install \| sh -s -- --binary"` |
| `pi` | its agent folder's `bin` (`~/.pi/agent/bin`) | `sh -c "curl -fsSL https://pi.dev/install.sh \| sh"`. Offered only where npm is here and `node --version` names 22.19 or newer within 5 s. Else refused, with the requirement that is missing. |
| `claude-code` | `~/.local/bin` | `bash -c "curl -fsSL https://claude.ai/install.sh \| bash"` |
| `codex` | `CODEX_INSTALL_DIR`, else `~/.local/bin` | `sh -c "curl -fsSL https://chatgpt.com/codex/install.sh \| CODEX_NON_INTERACTIVE=1 sh"` |
| `grok` | `~/.grok/bin` | `bash -c "curl -fsSL https://x.ai/cli/install.sh \| bash"` |
| `gemini` | npm's global folder (`npm prefix -g`) | `npm install -g @google/gemini-cli`. Offered only where npm is here and `node --version` names 20 or newer within 5 s. Else refused, with the requirement that is missing. |

When the driver does not offer `install`, `setup/plan` answers `refused` with the reason in one
line. `steery setup --harness <id>` shows this line. Two examples:

- "pi's installer needs Node.js 22.19 or newer and npm (`npm` is not on PATH)"
- "omp's installer needs `sh` and `curl` on PATH: `curl` is not there"

Without Node.js 22.19 or newer and npm, pi's installer exits 0 and installs nothing. Thus the
driver does not offer it there, and Steery never runs it there.

The driver can find the harness in more than one place: two folders on `PATH`, or one folder on
`PATH` and the installer's folder. A link to the same file counts as one place. Then
`setup/plan` says so in a note. The note names the executable that Steery runs (the first on
`PATH`, else the one in the installer's folder). It also names the others, which Steery never
runs. The protocol has no field for a list of binaries (`Found` has one `binary`). Thus the note
is the only place for this list.

## Signing in

`sign_in` tells a person what to do at the machine. The person reads it on their phone after
"<harness> is not logged in on <machine>:". `models`' `signed_in` tells whether the harness is
signed in, as the harness itself says.

| Driver | `sign_in` | `signed_in` |
|---|---|---|
| `omp` | ``run `omp login` in a terminal`` (`omp login` is the terminal counterpart of `/login`) | not told |
| `pi` | ``run `pi` in a terminal and type /login in it`` (pi has no login command. Its `auth` prints credentials and checks readiness.) | not told |
| `claude-code` | ``run `claude auth login` in a terminal`` | `claude auth status --json`'s `loggedIn` |
| `codex` | ``run `codex login` in a terminal`` | `codex login status`. Exit 0: signed in. Exit 1 with `Not logged in`: not signed in. |
| `grok` | ``run `grok login` in a terminal`` | `grok models`' first line. `You are logged in with …` or `You are using XAI_API_KEY.`: signed in. `You are not authenticated.`: not signed in. |

Each probe has a bound of 5 s. The driver kills a probe that runs past it. A probe gives no
answer in these cases:

- It did not start.
- It did not end in time.
- It exited in another way. For example, `codex login status` also exits 1 for a `config.toml`
  that it cannot read. Also, its `Not logged in` says nothing when `config.toml` names a
  `model_provider` of its own.

When the probe gives no answer, `signed_in` is absent and the driver claims nothing. The driver
writes the reason to standard error, which is the runtime's log of the driver. `Models` has no
field for a reason. To show the reason to a person, the SDK needs a field for it:
`signed_in_why?: string` beside `signed_in`. This is a protocol change.

When the harness is not signed in, `setup/plan` asks the `sign-in` need (protocol 5). It gives
its ways best first:

1. A way whose return the Steery app catches.
2. A code that the person types on the vendor's page.
3. A key.

A way that runs the harness's own login starts that login unmodified on a terminal of the
driver's ([`common/src/login.rs`](common/src/login.rs)). The driver answers with the login's
link as the next step. The driver keeps the login for 10 minutes. Only one login waits at a
time. When the link returns to a loopback address, the link's `fills` names the step's
`address` field. This field is secret and never required. Then `needs/give` takes one of these:

| Given | What the driver does |
|---|---|
| an address on the login's own port (`http://localhost:<port>/…`, `127.0.0.1`, `[::1]`) | GETs it from inside this host. The harness's own listener then completes the sign-in. |
| anything else | types it into the login's prompt, where it has one. Else refused, with the address to paste. |
| nothing | done when the login exited 0 within 20 s. Else refused, with a request to paste the address (or finish on the page). |

| Driver | Ways, in order | Next step's link |
|---|---|---|
| `claude-code` | `claude-sign-in`, `api-key` | the link that `claude auth login` gave the driver's `$BROWSER` (`localhost:<port>/callback`). Else its printed manual link. |
| `codex` | `chatgpt`, `device`, `api-key` (OpenAI) | `codex login`'s link (`127.0.0.1:1455`). `--device-auth`'s page, with its code in the step's label. |
| `omp` | one per provider below, `api-key` | the link that `omp login <provider>` prints. A code that it shows goes in the step's label. |
| `pi` | `api-key` | none |
| `grok` | `grok-sign-in`, `api-key` (xAI, kept as `XAI_API_KEY`) | `grok login --device-auth`'s page (`accounts.x.ai/oauth2/device?user_code=…`), with its code in the step's label. A key counts where `grok models` with it, in an empty `GROK_HOME`, fetched xAI's models. |

omp lists its providers' ids and names (RPC `get_login_providers`, the `omp login` picker). It
does not list how each provider signs in. Most providers only read an API key on their prompt.
Thus the driver keeps its own list of omp's OAuth providers (`pi-family/src/sign_in.rs`, omp
18.12.6):

- Return to an address: `openai-codex`, `google-gemini-cli`, `google-antigravity`,
  `openrouter`, `devin`, `stencil`, `gitlab-duo`, `gitlab-duo-agent` and `zai-coding-plan`.
  The last two return to an app's address, which the person pastes.
- Code on the vendor's page: `openai-codex-device`, `github-copilot`, `cursor`, `kimi-code`,
  `xai-oauth`, `muse-code`, `factory-droid` and `kilo`. The driver answers the GitHub
  Enterprise question of `github-copilot` with Enter: github.com.
- Left out: `anthropic` (its terms), `perplexity` and `snowflake` (each asks for an email or an
  account first).

## Building

The SDK and the protocol are in [steery-dev/sdk](https://github.com/steery-dev/sdk). Every
crate names the SDK by git, at one tag, in its `Cargo.toml`
(`steery-sdk = { git = "https://github.com/steery-dev/sdk", tag = "v<version>" }`). Every
crate pins its toolchain in `rust-toolchain.toml`.

```sh
git clone https://github.com/steery-dev/drivers
cd drivers/pi-family && cargo build
cargo test
```

Every build goes through [Kache](https://github.com/kunobi-ninja/kache), the compiler cache that
`.cargo/config.toml` names. Install Kache once on each machine:

- Mac: `brew install kunobi-ninja/kunobi/kache && kache daemon install`
- Linux: its [APT package](https://kunobi.ninja/docs/kache/getting-started/installation)

`./release.sh <tag>` does these steps:

1. It builds every driver with `--release --locked` and `cargo zigbuild`, for each target that
   Steery runs on, into `dist/<target>/`. The targets are `aarch64-apple-darwin`,
   `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`. The Linux
   builds link glibc 2.30 at most.
2. It packs them into `dist/release/` with
   `steery programs pack --release <tag> --out dist/release <target>=dist/<target>…`. The
   result is one archive per driver and target, and the manifest, `steery-programs.json`
   (PROTOCOL.md §7.1).
3. With `--publish`, it makes the GitHub release `<tag>` of those files (`gh release create`).
   It records the release on Steery's `drivers-releases` train: `build` as it starts, then
   `shipped`, or `failed` with the step that stopped it. Steery's `release-of-commit` reads that
   train to say which release first carries a commit.

With `--no-pack`, it stops after the build, and `--publish` needs the pack step. It needs these:

- `cargo-zigbuild`
- `zig`
- the four targets (`rustup target add`)
- `steery` on `PATH`, or named by `STEERY`; with `--publish`, signed in to the `steery-dev`
  workspace

## Checking a driver

```sh
steery programs test target/debug/steery-driver-omp
steery programs test target/debug/steery-driver-pi
```

`steery programs test [--setting NAME[=VALUE]]… <command> [args…]` starts the driver. It checks
these things, one line per check:

- what the driver says it is
- the methods that read and change nothing
- an id sent twice
- a method that the driver does not have
- its shutdown

The command exits 0 only when every check passes. A read that gets a refusal word that its
method lists counts as an answer. `--setting NAME=VALUE` passes a setting to `initialize`.
`--setting NAME` takes the value from the environment, so a secret stays off the command line.
The command skips a method that needs a required setting which is not given. `models` asks the
harness itself, so the harness must be on `PATH` for that check to pass. Each crate's own
`cargo test` covers what the crate reads and writes.

## Writing a driver

A driver for a harness that Steery does not ship is a program like these drivers. `pi-family/`
is the worked example. Copy it and keep its shape (the layout is below, under omp and pi).

- **Its crate** depends on `steery-sdk` by git, as above. It depends on `steery-driver-common`
  by path where it keeps a tree, follows a file or reads a receipt. In this repository this
  path is `../common`. In a copy elsewhere, the `common` crate goes beside the driver's crate.
  The crate uses `tokio` for its runtime and `serde_json` for the parameters. It declares an
  empty `[workspace]`, so it builds alone. Its binary has the name `steery-driver-<id>`.
  `src/bin/omp.rs` is one line, `steery_driver_pi_family::serve(Harness::Omp)`. This line calls
  `steery_sdk::serve(|_notifier| Driver::new(harness))`. The SDK reads requests and answers
  each one. It also gives the notifier, which the driver uses for what it says unasked.
- **`initialize`** (`Program::initialize` in `src/lib.rs`) answers with the newest protocol that
  the runtime offers and that the driver speaks (`steery_sdk::pick`). It refuses a runtime that
  offers none of them. It keeps the `home` and `config` that it gets. Its answer is
  `wire::Initialized`, with these fields:
  - `program`: `id`, `kind: Harness`, `release`, `label`, `short`, and optionally `tint` and
    `mark`. This is what a person sees of the driver.
  - `harness`: `HarnessInfo` from `src/harness.rs` (PROTOCOL.md §3.1). It holds the levels, the
    harness's minimum release, how the harness signs in, its facts, its hooks and its door.
  - no `machine`.
  - the settings that the driver needs (none here; §3.4).
- **The methods** (`Program::call`) are one match from each `steery_sdk::method` name to the
  module that answers it. The module reads its parameters with `steery_sdk::params`. It gives
  its answer with `steery_sdk::result`. The methods are:
  - observe (§4.1): `setup/plan`, `sessions/list`, `sessions/locate`, `sessions/read`,
    `hook/read`, `mcp/list`.
  - drive (§4.2): `held`, `idle`, `models`, `start`, `turn/read`, `receipt/*`, `reader`,
    `terminal`.
  - any other method answers `Error::Unknown`.

  A refusal is `Error::refused` with one of the protocol's words (`steery_sdk::refusal`).
- **Live** (§4.3) is a door. A harness with its own door (`picks`, `keeps`) answers `say`,
  `pick` and `stop` in its driver. `claude-code` and `codex` do this. omp and pi have
  no door of their own, so their door is `plugin`. This repository keeps Steery's plugin in
  `pi-family/plugin/`, and `setup/plan` installs it. The plugin speaks the plugin door,
  version 1 (§4.4), to the runtime itself. Thus the driver has no live methods. The runtime
  uses the live methods only on conversations that Steery started. The runtime never steers a
  conversation that a person started.

## MCP servers

`mcp/list` (protocol 4) answers with the MCP servers that a conversation loads:

- Without `cwd`: the person's own servers.
- With `cwd`: the servers that a conversation started in that folder loads, as the harness
  itself decides.

For each server, only its name leaves the machine. For a URL server, the URL's scheme, host,
port and path also leave the machine. The file [`common/src/mcp.rs`](common/src/mcp.rs) reads
the `mcpServers` files that omp, pi and Claude Code share.

| Driver | Everywhere | In a folder, ahead of those | `complete` |
|---|---|---|---|
| `omp` | agent folder's `mcp.json`, `.mcp.json`. `enabled: false` off unless in `enabledServers`. `disabledServers` off. | `.omp/mcp.json`, `.omp/.mcp.json`. The folder's `mcp.json`, `.mcp.json` last. None where `config.yml` says `mcp.enableProjectConfig: false`. | never: other tools' files, plugins, extensions |
| `pi` | agent folder's `mcp.json` | `.pi/mcp.json`, where `trust.json` trusts exactly that folder | never: extensions register servers |
| `claude-code` | `~/.claude.json` `mcpServers` | local scope `projects["<folder>"]`. `.mcp.json` where the folder is trusted and the server approved, not rejected. (`start` runs `claude --bg`, not `-p`. `claude --bg` is an interactive background session that waits on approval.) `disabledMcpServers` off. | not with managed configuration, a plugin on, or a claude.ai sign-in |
| `codex` | `codex mcp list --json` run at `/` | the same, run in the folder | never: account connectors |
| `grok` | `grok inspect --json` run at `/`: `config.toml`, `~/.claude.json`, `~/.cursor/mcp.json`. A server imported from another tool and turned off there is left out. | the same, run in the folder: `.grok/config.toml` where trusted, `.mcp.json` | never: plugins, servers an ACP client forwards |

## Adding a driver

Steery adds a driver in the same way as every program, Steery's own included (PROTOCOL.md §7.2):

```sh
steery programs add https://github.com/steery-dev/drivers
steery programs add https://github.com/me/my-harness@v0.1.0 --setting MY_HARNESS_TOKEN
```

`<source>` is one of these:

- a GitHub repository: its latest release, which `steery programs update` follows, or
  `@<tag>` for that release
- a release's manifest, `steery-programs.json`, by URL or path
- one program's archive
- a built `steery-driver-<id>` itself

To publish your own driver, do what this repository does:

1. Build the driver for each target.
2. Pack it with `steery programs pack`.
3. Make the release of those files.

Steery builds nothing on the computer that it adds the driver to.

For each program, Steery does these steps with the build for this computer:

1. It fetches the build.
2. It checks the build against its sha256.
3. It copies the build into Steery's programs folder.
4. It starts the build and checks it as `steery programs test` checks it, with the settings
   given.

A driver's settings (PROTOCOL.md §3.4) are, for example, an API key or the account that the
driver acts as. The person gives them here once: `--setting NAME=VALUE`, or `--setting NAME` to
take the value from the environment. When the person does not give a secret setting, they type
it at the terminal, with no echo. Steery prints what it will add and asks once at the terminal.
`--yes` gives that answer in all other places. The runtime sends the settings in each
`initialize` that it sends the driver. The runtime runs as a service (launchd, systemd), and
the environment of this service holds none of the person's shell variables. Thus a driver reads
nothing that it needs from its environment.

`steery programs remove <id>` removes it.

## omp and pi: `pi-family/`

omp is a fork of pi. Both harnesses do these things:

- write the same transcript
- load the same plugin
- read the same `PI_*` variables for their folders
- run as kept processes in their RPC mode

Thus one crate holds both drivers: a library, and two binaries, `steery-driver-omp` and
`steery-driver-pi`. Each binary is one line that serves the library for its harness. Neither
driver has settings.

**Needs** (protocol 5: a driver that picked version 4 plans no needs). When the harness lists
no model, the plan asks `sign-in`. For omp, this is when `omp models --json` prints
`{"models":[]}`. For pi, this is when `pi --list-models` prints "No models available". The
ways are:

1. In omp only: one way per OAuth provider that `omp login <provider>` signs in to, by its id
   (see Signing in).
2. In both: `api-key`. This is an Anthropic API key. The driver checks that it starts with
   `sk-ant-api`. The driver returns it as `ANTHROPIC_API_KEY`, which both harnesses read.

The driver never offers omp's and pi's own Claude login. That login signs a third-party app in
to a Claude plan, and Anthropic's terms forbid this. Because pi has no login command, pi has only
the key.

What each driver does:

- **Conversations** (`sessions/list`, `sessions/locate`, `sessions/read`). The driver reads them
  from the transcripts under the agent folder, in `sessions/<folder>/<started-at>_<id>.jsonl`.
  The agent folder is `~/.omp/agent`, `~/.pi/agent`, or `PI_CODING_AGENT_DIR`. For omp,
  `PI_CONFIG_DIR` and `OMP_PROFILE` also change the agent folder. The driver also reads
  transcripts from these folders:
  - the folder that `PI_CODING_AGENT_SESSION_DIR` names
  - the folder in pi's `sessionDir` setting
  - the folders of the transcripts that the harness's moments named

  A transcript is a tree: each entry names its parent. The conversation is the path from the
  last entry back to a root. The driver reads only that path. A conversation can go back to an
  earlier entry and continue from there. Then the next read starts again from the beginning
  (`reset`). The events are these:
  - turns and how they ended
  - words
  - calls, grouped as Steery groups them
  - tokens per response
  - models (in pi with their thinking level)
  - forks
  - omp's sub-agents

  A failed model call ends its turn only when the harness does not continue after it. The
  driver still gives the tokens that an abandoned branch used, once, as the `tokens` of each of
  its responses.
- **Setup** (`setup/plan`). The plan has these entries:
  - the skill
  - Steery's plugin
  - in omp, the MCP entry in `mcp.json`

  The plugin is a loader in `extensions/steery.ts`, plus the copy of the extension that the
  loader runs. The runtime's config folder keeps this copy. In pi, the plugin carries the MCP
  entry and registers it for each conversation, with headers that name that conversation. The
  entry has `exposure: direct`, so pi gives its model each tool by name. A `steery` server in
  pi's own `mcp.json`, or in the `.pi/mcp.json` of a folder that pi trusts, wins over the
  plugin's entry, with no headers. The plan notes each such server that it finds. A pi process
  that starts while the runtime is down waits its own 10 s for the entry. Then it runs without
  Steery. The plugin connects the entry again when the door is back. When omp and pi are both
  installed and share one folder, the plan writes no setup for either harness.
- **Moments** (`hook/read`). The plugin forwards each moment that the harness gives it over the
  door. The driver reads it as a Moment.
- **Drive** (`held`, `idle`, `models`, `start`, `turn/read`, `receipt/*`, `reader`,
  `terminal`).
  - `held`: there is no registry, so the runtime looks for processes. In pi, the runtime also
    looks for a stray process, because a pi process without the plugin does not hold a file
    open.
  - `models`: omp's models come from `omp models --json`. When omp's `enabledModels` names
    models, the driver gives only those. The default comes from `config.yml`, or from
    `config.yaml` when that is the only file there. The omp harness itself reads both, in that
    order, and writes the one that it read. The code of omp names `config.yaml` its
    legacy-compatible fallback. For pi, the models come from `pi --list-models` and its RPC
    mode's `get_available_models`.
  - `start`: a turn is a kept process, `--mode rpc` (omp's with `--no-ui`), with its standard
    input held. The process stays alive between turns. It takes the words of every turn, the
    first turn included, through the plugin door. For omp, a turn continues by id
    (`--resume`). Before the launch, the driver checks a model against omp's catalog. For pi,
    a turn continues by transcript (`--session <file>`). The pi command also passes a model to
    the plugin (`--steery-model`). The plugin then ends the process with a refusal when pi
    does not run that model exactly. Neither harness makes a worktree for a turn. Thus a fork
    or a new conversation that asks for a worktree (`facts.worktrees`) starts in a worktree
    that the driver makes, detached at `HEAD`. The driver makes it where omp keeps its own
    worktrees: under `OMP_WORKTREE_DIR`, else `~/.omp/wt` (for pi, `~/.pi/wt`), as
    `<4 hex>/<repository>`. A turn in a folder that is in no repository runs in that folder.
  - `turn/read`: the harness's own refusals on standard error. The runtime discards the
    harness's standard output. The door says what the harness does.
  - `receipt/mark`, `receipt/find`: the person's message that carries the delivery marker. They
    look in the transcript after a mark. When the harness rewrote the file after the mark, they
    look in all of it (some omp compactions do this). Words that the door gives during a turn go
    in at the next tool.
  - `reader`: a one-shot `--print` reader. It closes everything that a flag can close.
  - `terminal`: the command line that resumes a conversation in a terminal.

The live methods (`say`, `pick`, `stop`, `force`, `compact`) belong to the plugin door
(PROTOCOL.md §4.4). The runtime speaks this door itself, so the drivers have no live methods.
The door answers a `say` when the harness holds the words, with the time in `queued_at`. When
the runtime asks the same `say` again, the door answers when the harness took the words into
its turn (`taken_at`). Both omp and pi take a steer only between tool calls. `force` aborts the
running turn, so the held words go in next, in the same process. At an abort, omp puts the words
that are still queued into a turn of their own.

The door's `state` says `compacting` for every compaction that the harness runs:

- pi: from its `session_before_compact` to its `session_compact` (or `session_compact_failed`).
- omp's own compaction: from `auto_compaction_start` to `auto_compaction_end`.
- a compaction that something asks of omp (`/compact`, an extension's): from
  `session_before_compact` to `session_compact`. When this compaction fails, `compacting` ends
  at omp's next run. omp tells an extension nothing about the failure.

The door's `model` and `thinking` come, in pi, from `model_select` and `thinking_level_select`.
omp says neither. Thus the door reads omp's own model and thinking every second while the door
is open.

### Layout: a template for a harness Steery does not ship

To connect another harness, copy `pi-family/` and keep its shape:

| File | What it holds |
|---|---|
| `src/bin/<id>.rs` | The binary: `serve(Harness::…)`. |
| `src/lib.rs` | `initialize`, and one match from each method to the module that answers it. Methods that the harness cannot do answer `Error::Unknown`. |
| `src/harness.rs` | Everything about the harness as Steery reads it: id, `HarnessInfo` (levels, minimum, identity, facts, hooks), its folders, its executable. Every other module asks here, and never names a harness itself. |
| `src/transcripts.rs` | Where the harness files conversations. `sessions/list`, `locate`, `read`, and the cursor's `mark`. `steery_driver_common::tree` holds the rest of the cursor and the batched read. |
| `src/tree.rs` | The transcript's format: here, how pi's entries name their parent, as a `steery_driver_common::tree::Shape`. |
| `src/events.rs` | One record → the protocol's events. |
| `src/calls.rs` | A tool call → `Call`: its group, MCP server, Steery's own tool, the questions it asks. |
| `src/hooks.rs` | `hook/read`: a hook's document → `Moment`. |
| `src/models.rs` | `models`: what the harness lists, and the files whose change changes it. |
| `src/drive.rs` | `held`, `idle`, `start`, `turn/read`, `receipt/*`, `reader`, `terminal`. |
| `src/setup.rs` | `setup/plan`: the entries, each with what it carries. |
| `plugin/` | Steery's plugin for harnesses whose door is `plugin`: the extension and its loader. The build embeds them in the binary. |

A harness with its own door (`picks`, `keeps`) has no `plugin/`. Its driver answers `say`,
`pick` and `stop` itself. A driver whose harness has hook commands as moments also answers
`hook/answer`.

### The plugin

`pi-family/plugin/extension.ts` speaks the plugin door, version 1. It draws nothing in the
harness and takes no answer there. `loader.ts` is the file that the harness loads. It runs the
copy of the extension that the runtime's config folder names. When the config folder names a
newer copy, the loader moves every session onto it. Typecheck the two files against pi's
published types:

```sh
cd pi-family/plugin && npm i && npm run check
```

## Claude Code: `claude-code/`

One crate, one binary: `steery-driver-claude-code`. It has no settings. Claude Code keeps
everything under its configuration root, `CLAUDE_CONFIG_DIR` or `~/.claude`.

**Needs** (protocol 5: a driver that picked version 4 plans no needs). When
`claude auth status --json` says `"loggedIn": false`, the plan asks `sign-in` in two ways. An
`ANTHROPIC_API_KEY` in Claude Code's environment counts as signed in. The ways are:

1. `claude-sign-in`, offered first. The driver runs the unmodified `claude auth login` on a
   terminal of its own, with `BROWSER` naming a script of the driver's. This script records the
   link that Claude Code opens (`redirect_uri=http://localhost:<port>/callback`). The driver
   answers this link as the step `claude-sign-in-return`, and the address fills the step's
   field. Sometimes no `$BROWSER` call comes within 3 s of the printed manual link. Then the
   step has the manual link. Its page shows a code, and nothing fills the field.
2. `api-key`: an Anthropic API key. The driver checks that it starts with `sk-ant-api`. The
   driver returns it as `ANTHROPIC_API_KEY`. Claude Code uses that variable unasked only in
   `-p` (the reader). A `--bg` session uses it only after approval. Thus a turn that holds the
   key passes `--settings '{"apiKeyHelper": "printf %s \"$ANTHROPIC_API_KEY\""}'`, and writes
   nothing.

Claude Code keeps the sign-in on this host. The driver keeps no token. A driver that started
again has no sign-in that waits. Thus a step at that time is `not-found`.

- **Conversations** (`sessions/list`, `sessions/locate`, `sessions/read`). The driver reads them
  from `projects/<folder>/<id>.jsonl` under the root. It also reads them from the `projects`
  folder of any transcript that the harness's moments named. A resumed conversation continues
  in its own file. Claude Code files a sub-agent's transcript under the transcript of its
  conversation, as `<id>/subagents/agent-<agent>.jsonl`. The `.meta.json` beside it names its
  type, its task and the call that started it. A run that a program started
  (`entrypoint: sdk-cli`) is not a conversation until a person continues it. A run that the
  Claude app started (`entrypoint: claude-desktop`) is `app`. The events are these:
  - turns and how they ended: an API error with its status and code, and the time when a limit
    ends, where the error says so
  - words
  - calls, grouped as Steery groups them
  - tokens per response
  - models
  - sub-agents that its lead started, and how each ended, as Claude Code's
    `<task-notification>` told it. The notification can come as a person's message or as a
    queued attachment.

  A sub-agent's own turn ends where it returns its report (`SubagentHandback`). An API error
  ends its turn only when Claude Code does not continue after it. The transcript can still get
  the answer of a retry. While it can, the error and the records after it wait for the next
  read, as in omp.
- **Setup** (`setup/plan`). The plan has these entries:
  - Steery's MCP entry in `.claude.json` (beside the root, when the root is `~/.claude`)
  - the skill in `skills/steery/SKILL.md`
  - one hook group per event in `settings.json`, at `/hooks/<event>/-`, beside the person's
    own groups. Steery's group runs `steery hook claude-code <event>`

  `found.release` is what `claude --version` says.
- **Moments** (`hook/read`, `hook/answer`). A hook's document becomes a Moment, with the state
  that Claude Code's documents carry:
  - the model (`SessionStart`'s `model`, `PostModelSwitch`'s `to_model`)
  - the effort (`effort.level`)
  - at `StopFailure`, the failure (`error`, with its words in `last_assistant_message`)

  The documents carry no time. `PreCompact`/`PostCompact` carry no compaction id or tokens. Thus
  compactions come only from the transcript. Steery's answer becomes what Claude Code reads:
  - `hookSpecificOutput.additionalContext`
  - a `Stop` that keeps the turn running (`decision: block`)
  - a `PreToolUse` or `PermissionRequest` decision
  - the conversation's lead, written to `CLAUDE_ENV_FILE` (`append`)

  Cursor also runs the hooks in Claude Code's `settings.json` files, under its own chat id. Its
  document has `cursor_version`, and the hook's process has `CURSOR_VERSION`. Every hook entry
  names `CURSOR_VERSION` in `env`, and no other Cursor variable. For either sign, `hook/read`
  gives no moment and `hook/answer` prints nothing. Thus a Cursor turn makes no Claude Code
  session, and Steery never allows or blocks Cursor's tools.
- **Drive** (`held`, `idle`, `stop`, `receipt/*`, `models`, `start`, `turn/read`,
  `reader`, `terminal`).
  - `held`: Claude Code's registry, `sessions/<pid>.json`, names the live session in a
    conversation. Only its owner can stop a background session: the driver runs `claude stop`
    with the session's owner token. A terminal is a process that the runtime stops. When no
    session is registered, the runtime looks for processes that hold the transcript.
  - `idle`: what `claude agents --json --all` lists, with the registry.
  - `stop`: for a background session, the driver first runs `claude stop <job>`, which keeps
    the conversation. Then it stops every command that the session's process started and that
    outlived it. The shells of its tools run in their own process groups, and `claude stop`
    leaves them running. The driver sends SIGTERM to each command and to the group that the
    command leads. After 500 ms, it sends SIGKILL to what is still there. A stop that names a
    say (protocol 7) ends the session only when the running turn took that say's words. When
    another turn runs, or the words still wait in the inbox, the stop is refused
    `other-turn`. When no turn runs, the answer is `stopped: false`.
  - `start`: a turn is a background session (`claude --bg`, `--resume`, `--fork-session`,
    `-w`, `--model`, `--effort`). The plan runs this driver's own binary in its turn mode
    (`steery-driver-claude-code turn …`). The turn mode starts the session. It follows the
    session through `claude agents` to the point where its turn stopped. `turn/read` reads
    those lines and Claude Code's own refusals (`untrusted`, `not-found`, `harness-busy`,
    `selection-unsupported`). The launch gives that session its own `Stop` and `StopFailure`
    hooks (`--settings`). These hooks run the binary's `heard` mode into a file that the
    follower reads. The file tells whether a stop that the listing shows belongs to this turn.
    It also holds Claude Code's words for an API error. Each line that the turn mode writes
    also tells whether the session's transcript holds the person's input with the words'
    delivery marker yet. The turn mode looks only after the point where the transcript ended
    before the launch. `turn/read` gives that as `took` (none for words with no marker).
    Steery's own variables (`STEERY_STARTED`) reach the session in the same settings, as
    `env`. They never go through the launch's environment. Claude Code's supervisor keeps the
    environment of the launch that started it, for every later session.

    Claude Code can keep the background job of a conversation (`jobs/<id>/state.json`). A
    continue of that conversation wakes the job with `claude --bg --resume <id>` and no other
    flag, because any flag beside it starts a copy. The job continues with the settings that
    its first launch gave it. The driver first refuses a model or an effort that the job would
    not run (`selection-unsupported`). When Claude Code starts a copy anyway, the driver stops
    and removes the copy. It refuses the turn in Claude Code's words. When the runtime stops a
    turn (a signal to the turn mode), the turn mode ends its session through `claude stop`.
    This keeps the conversation. Then it stops the commands that the session left running, as
    `stop` does. It does this because no signal of the runtime reaches a process of the
    supervisor.
  - `receipt/mark`, `receipt/find`: the person's input that carries the delivery marker, in the
    transcript after a mark.
  - `models`: Claude Code's documented aliases (`opus`, `sonnet`, `haiku`, `fable`, `best`,
    `opusplan`, `opus[1m]`, `sonnet[1m]`). The model in `settings.json` comes first. The list
    holds only the models that `availableModels` allows. This is the managed list where one is
    set, else the user's `settings.json`. An alias is in the list only when every model that it
    can run is allowed. Each model in the list has Claude Code's effort levels. `signed_in`
    comes from `claude auth status`. The driver watches the files that a login rewrites
    (`.claude.json`, `.credentials.json`).
  - `reader`: `claude --print` with no tools, no MCP server, `--safe-mode`, and no session
    kept.
  - `terminal`: `claude --resume <id>`, or `claude attach <job>` while a background session
    holds the conversation. Also the Claude app's link for a conversation that the app started.
- **Live** (`say`, `session/changed`) is for a conversation that Steery started and that a
  Claude Code session holds.
  - Words go into the inbox socket that the registered session opens, after its peer token.
    The socket says nothing back. Thus the words are unconfirmed (`took: false`), and the
    runtime finds them with `receipt/find`.
  - The driver keeps a say with an `id` (protocol 7) together with the point where the
    transcript ended when its words went in. The transcript then names the turn that took the
    words. This is a `user` record that begins the turn, or a queued prompt that a running turn
    reads between its calls. `session/changed` names the say in that turn's `says`, and in
    `settled` after the turn ended.
  - The driver reads the transcript as it grows. The transcript also gives `retrying`, `jobs`
    and a dialog's title.
  - `retrying`: true while a running turn waits for the `retryInMs` of its newest `api_error`
    record that no answer followed. `retry` holds the record's `retryAttempt`, `maxRetries`
    plus one, and the error's `formatted` words. `LiveRetry` has no field for the time of the
    next try.
  - `jobs`: the shells, monitors and sub-agents that the main thread's calls left running
    (`backgroundTaskId`, a monitor's `taskId`, an `async_launched` `agentId`). The driver
    counts only jobs from the time when the process that holds the session took it (its
    registry entry's `startedAt`). Work ends with the process that ran it, and that end tells
    nothing. A job stays in `jobs` until a `<task-notification>` tells its end, or until a
    `TaskStop`/`KillShell` call stops it. The notification tells the end by a final `status`.
    For a monitor, it tells the end by an `event` that says the monitor expired, timed out or
    was stopped. Claude Code files the notification as a queued command in a running turn, or
    as the words of a turn of its own.
  - A `permission prompt` dialog's title, `Allow <tool>: <command or path>?`. It comes from the
    first call with no result in the newest answer. Claude Code asks about an answer's calls in
    its order. When that call is a sub-agent's, the title does not name the call.
  - The driver does not count work that started before the newest answer to which a newly
    started driver reads back.
  - Nothing that Claude Code writes shows a compaction while it runs. Its `compact_boundary`
    comes only after the compaction is done. Thus `compacting` stays false.
  - The session keeps its own model, so `pick` is unsupported.

```sh
cd claude-code && cargo build
steery programs test target/debug/steery-driver-claude-code
cargo test
```

`cargo test` needs no Claude Code. The reader's test runs only where `claude` is on `PATH`.

## Codex: `codex/`

One binary, `steery-driver-codex`, with no settings. It speaks to Codex only through what Codex
publishes:

- its rollout files
- `hooks.json` and `config.toml`
- `codex app-server` (its JSON-RPC over standard input and output)
- `codex app-server proxy` (the bytes of a running app server's control socket)
- the Codex app's router `<CODEX_HOME>/ipc/ipc.sock`
- `codex exec`
- `codex mcp list` and `codex features list`

Codex's home is `CODEX_HOME`, else `~/.codex`.

**Needs** (protocol 5: a driver that picked version 4 plans no needs). When
`codex login status` says not logged in (see Signing in), the plan asks `sign-in` in three
ways. The driver runs each way on a terminal of its own, in Codex's home.

1. `chatgpt`: `codex login`. Its printed loopback link (`127.0.0.1:1455`) is the step
   `chatgpt-return`. The address fills this step.
2. `device`: `codex login --device-auth`. Its page and one-time code are the step
   `device-code`, with no field.
3. `api-key`: an OpenAI API key. `codex login --with-api-key` reads it on standard input, and
   Codex keeps it.

When Codex's own `hooks/list` lists a Steery hook as `untrusted` or `modified`, the plan also
asks `hooks` in one way, `trust`. Codex runs a hook only after its person trusted the hook's
exact content. Until then a Steery turn cannot ask its person for more permissions (Codex
refuses the escalation with no one to ask), and Codex hands no moment over. On the person's
Go, the driver writes Codex's own trust for each such hook at its current hash, through the
app server's `config/batchWrite` in Codex's home (`hooks.state.<key>.trusted_hash`). It
trusts only the hooks whose command is the runtime's hook line from the last plan. Where
every Steery hook runs already, the way is `not-found`. A hook that an administrator manages
is not the person's to trust. A driver on version 4 says the same as a note.

- **Conversations** (`sessions/list`, `sessions/locate`, `sessions/read`). The driver reads
  rollouts from `sessions/<yyyy>/<mm>/<dd>/rollout-<started-at>-<thread>.jsonl`. A thread that
  continues in more than one rollout is one conversation. Its newest rollout is the current
  one. When two rollouts are equally current, they name the conversation ambiguously
  (`complete: false`). The driver lists a sub-agent's rollout under its lead, never as a
  conversation. (The `session_id` of that rollout names another thread.) The driver also does
  not list a `codex exec` run as a conversation. The driver reads a fork's history
  (`history_base`) from its parent's rollout, up to the point where the fork took it. Then comes
  a `fork` event, then the fork's own rollout. The events are these:
  - turns and how they ended (a failure is classified by its status or by Codex's own code)
  - the person's words, with the files and pictures that the person attached (with no
    fragments of Codex's own)
  - the agent's words and reasoning
  - calls and their results
  - each response's own tokens (`token_usage_record`, by its response id, cache writes
    included)
  - the running total, with the context left
  - compactions, with the context before and after them. The context after is the count that
    Codex makes right after the compaction. Codex records no reason.
  - models and efforts
  - sub-agents that started, and how they ended
  - rollbacks

  The driver refuses a rollback that does not say how many turns went.
- **Setup** (`setup/plan`). The plan has these entries:
  - The MCP entry in `config.toml` (`mcp_servers.steery`). Its `tool_timeout_sec` is the
    runtime's bound. It has `default_tools_approval_mode = "approve"`, because under
    `approval_policy = "never"` Codex refuses a tool that it would ask about.
  - The rule `rules/steery.rules`, which runs `steery` outside Codex's sandbox. The macOS
    sandbox denies the Keychain, so inside the sandbox the CLI cannot reach this machine's key.
  - The skill in `skills/steery/SKILL.md`.
  - One hook group per moment in `hooks.json`, beside the person's own (`/hooks/<Event>/-`).

  Codex runs a hook only after its person trusted it. Codex keys that trust by the hook's
  place. Another tool can put a group ahead of a Steery hook that the person trusted, and so
  move it. Then the plan writes the same trust again at the hook's new place. The plan asks
  Codex's own `hooks/list` where the hook now stands, and never computes it here.
- **Hooks** (`hook/read`, `hook/answer`). Codex's documents become Moments. The driver keeps the
  model that each document names as state. An answer becomes what Codex parses:
  - `additionalContext` on `SessionStart` and `UserPromptSubmit`
  - `decision: "block"` at `Stop`
  - a `permissionDecision` on `PreToolUse`
  - a `decision` with its `behavior` (and the standing permission chosen) on
    `PermissionRequest`
- **Drive.**
  - `held`: a native owner, where one takes the thread's words. The owner is the window of the
    Codex app that owns the thread. Or it is a running app server of its home that has the
    thread loaded with nothing queued. Else the runtime looks, because an app server that holds
    the thread is out of reach.
  - `idle`: the owner's own state. Else, while nothing holds the rollout, the state of the
    rollout's newest turn. It gives the model and thinking that the thread runs on, and when its
    newest turn began and ended. The model and thinking come from the running app server's
    `thread/read` where one has the thread loaded. Else they come from the rollout's newest
    `turn_context`.
  - `start`: this binary in its turn mode,
    `steery-driver-codex turn --codex <codex> <continue|fork|new> [<thread>] [--worktree] [--model m] [--thinking t] --inbox <socket>`.
    A continue is refused `no-place` where the folder that the thread opened in is gone:
    Codex resumes a thread only there, and every command of the turn would fail to start.
    The turn mode does these steps:
    1. It opens the thread (`thread/resume`, `thread/fork`, `thread/start`). Where the
       person's configuration names no sandbox for the folder (`config/read`), it asks for
       `workspace-write`, as the Codex app does for each thread it starts. Codex otherwise runs
       a folder that nobody trusted `read-only`, and the turn writes nothing.
    2. It starts the turn.
    3. It writes every message of the app server to standard output.
    4. It takes more words on the inbox (`turn/steer`, or `turn/start` after the turn ended).
    5. It ends with the last turn.

    A worktree goes where Codex makes its own, `<CODEX_HOME>/worktrees/<4 hex>/<repo>`.
  - `turn/read`: the thread named, the words taken (by their delivery marker), and how each
    turn completed. Also the app server's refusals: `harness-busy` beside a live writer, and
    `model-missing` for a turn that failed `modelNotFound` (a model that Codex lists, but
    that the account does not get). The events of its lines go to the runtime before the
    driver reads the rollout. Each event has the rollout's own id. The events are these:
    - turns that began and ended
    - the agent's words and reasoning
    - shell and MCP calls and their results
    - the running token total
    - the model (rerouted included)
    - the thread's title (`thread/name/updated`)

    The driver leaves a person's words and a compaction to the rollout. The app server names
    them by ids that the rollout does not keep.
  - `stop`: the owner interrupts its own turn, and the driver sees the turn end. A stop that
    names a say (protocol 7) interrupts only the turn that the say's words went into. It goes
    through the window or app server that owns the thread now, also for a terminal Codex's
    thread. When another turn runs, or the driver cannot tell which turn runs, the stop is
    refused `other-turn`. When no turn runs, the answer is `stopped: false`.
  - `receipt/mark`, `receipt/find`: the thread's turns before the words, and the one turn after
    them that carries the delivery marker.
  - `models`: the app server's `model/list`, the default first. `signed_in` comes from
    `codex login status` (see Signing in). The driver watches `auth.json`, so it sees a login
    at once.
  - `reader`: `codex exec --json --ephemeral` with every MCP server off, no tool or feature
    that it does not need, and a read-only sandbox. `reader/read` takes its last agent
    message.
  - `terminal`: `codex resume <thread>`, and the app's `codex://threads/<thread>`.
- **Live** (`say`, `pick`, and `session/changed`) is for a conversation that Steery started.
  - Words go to the window that owns the thread. The window takes them as words typed into its
    composer. Else they go into the running app server's queue. The queue's acknowledgement is
    not delivery (`took: false`).
  - The driver ties a say with an `id` (protocol 7) to the turn that took its words. This is
    the turn that Codex named when it took them. Else it is the turn whose person input carries
    the say's delivery marker, once the thread's history has it. `session/changed` names the say
    in that turn's `says`, and in `settled` after the turn ended.
  - The driver checks a pick against the list of an app server of its own. For a thread that a
    running app server has loaded, the driver sets the pick as the thread's settings
    (`thread/settings/update`). For a thread that a window owns, the driver keeps the pick. It
    sends the pick with the next words that Steery says there (`thread-follower-start-turn`
    carries model and effort). The driver reads it back as picked.
  - The driver follows each thread that the runtime asks about, and says it as
    `session/changed`. For a window, it follows the window's own state of the thread through
    the router. A running app server can have a thread loaded: the window's own, or one with
    nothing but its queue. The driver follows that thread through that app server. It looks at the
    thread every second, and at once on `thread/status/changed`. It never subscribes to the
    thread. An app server sends a thread's approvals to every client subscribed to it, and this
    client refuses them.
  - The live state carries these:
    - the turn
    - `pending` (the queue, `thread/queue/list`)
    - the model and thinking
    - the context left
    - the calls that run, and the calls that ended lately (from the rollout's newest records)
    - `settled`

Build and check it, with Codex on `PATH`:

```sh
cd codex && cargo build
steery programs test target/debug/steery-driver-codex
cargo test
```
## License

MIT, see [LICENSE](LICENSE).
