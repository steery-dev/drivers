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
| `grok` | Grok Build | [`acp-family`](acp-family/) | observe, drive, live (Grok's leader: `picks`) |
| `opencode` | OpenCode 2 | [`acp-family`](acp-family/) | observe, drive, live (Steery's plugin door) |
| `hermes` | Hermes Agent | [`acp-family`](acp-family/) | observe, drive, live (Hermes's gateway: `picks`) |
| `kilo` | Kilo | [`acp-family`](acp-family/) | observe, drive, live (Steery's plugin door) |
| `copilot` | Copilot | [`acp-family`](acp-family/) | observe, drive, live |
| `cursor` | Cursor | [`acp-family`](acp-family/) | observe, drive |
| `cline` | Cline | [`acp-family`](acp-family/) | observe, drive, live |
| `goose` | Goose | [`acp-family`](acp-family/) | observe, drive |
| `amp` | Amp | [`amp`](amp/) | observe, drive, live (Steery's plugin door) |
| `gemini` | Gemini CLI | [`acp-family`](acp-family/) | observe, drive, live (Steery's plugin door) |
| `qwen` | Qwen Code | [`acp-family`](acp-family/) | observe, drive |
| `antigravity` | Antigravity | [`acp-family`](acp-family/) | observe, drive, live (Antigravity's own door: `keeps`) |
| `openclaw` | OpenClaw | [`acp-family`](acp-family/) | observe, drive |
| `droid` | Droid | [`acp-family`](acp-family/) | observe, drive |
| `crush` | Crush | [`crush`](crush/) | observe, drive, live (Crush's server: `keeps`) |

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
| `copilot` | `~/.local/bin` | `bash -c "curl -fsSL https://gh.io/copilot-install \| bash"` |
| `opencode` | `~/.opencode/bin` | `bash -c "curl -fsSL https://opencode.ai/v2/install \| bash -s -- --no-modify-path"`. OpenCode 2 only: an `opencode` that names a 1.x release does not count as found, and a note names it. |
| `cursor` | `~/.local/bin` | `sh -c "curl -fsS https://cursor.com/install \| bash"` |
| `amp` | `AMP_HOME/bin`, else `~/.amp/bin` (and its link `~/.local/bin/amp`) | `bash -c "curl -fsSL https://ampcode.com/install.sh \| bash"` |
| `cline` | npm's global folder | `npm install --global cline`, offered where npm is on `PATH` |
| `goose` | `GOOSE_BIN_DIR`, else `~/.local/bin` | `bash -c "curl -fsSL https://github.com/aaif-goose/goose/releases/download/stable/download_cli.sh \| CONFIGURE=false bash"`, offered where bash, curl and tar are on `PATH` |
| `gemini` | npm's global folder (`npm prefix -g`) | `npm install -g @google/gemini-cli`. Offered only where npm is here and `node --version` names 20 or newer within 5 s. Else refused, with the requirement that is missing. |
| `qwen` | npm's global folder (`npm prefix -g`) | `npm install -g @qwen-code/qwen-code`. Offered only where npm is here and `node --version` names 22 or newer within 5 s. Else refused, with the requirement that is missing. |
| `antigravity` | `~/.local/bin` (it adds the folder to the shell's profile) | `bash -c "curl -fsSL https://antigravity.google/cli/install.sh \| bash"`, offered where bash and curl are on `PATH` |
| `openclaw` | npm's global folder, else `~/.npm-global/bin` (the installer's user prefix), else `~/.local/bin` (its git install's wrapper) | `bash -c "curl -fsSL https://openclaw.ai/install.sh \| bash -s -- --no-onboard --no-prompt"`, offered where bash and curl are on `PATH` |
| `droid` | `~/.local/bin` | `sh -c "curl -fsSL https://app.factory.ai/cli \| sh"`, offered where curl and sh are on `PATH` |
| `crush` | Homebrew's `bin` (`/opt/homebrew/bin`, `/usr/local/bin`, `/home/linuxbrew/.linuxbrew/bin`) | `brew install charmbracelet/tap/crush` where `brew` is on `PATH`, else `npm install --global @charmland/crush` where npm is. Else refused. |

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
| `copilot` | ``run `copilot login` in a terminal`` | its own server's (`copilot --headless --stdio`) `auth.getStatus` `isAuthenticated`, bound 10 s. It counts a `gh` sign-in and `COPILOT_GITHUB_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN`. |
| `opencode` | ``run `opencode auth login` in a terminal`` | not told. OpenCode's free models need no account, so an empty model list never means signed out. |
| `cursor` | ``run `cursor-agent login` in a terminal`` | `models` runs `cursor-agent models`: `Authentication required` means not signed in. `cursor-agent status` says signed in while turns refuse, so the driver does not ask it. |
| `amp` | ``run `amp login` in a terminal`` | `amp threads list --json --limit 1`. Exit 0: signed in. Exit 1 with `Run 'amp login'`: not signed in. `setup/plan` asks the same, and an `AMP_API_KEY` counts as signed in. |
| `cline` | ``run `cline auth` in a terminal`` | `models` starts `cline --acp`: `session/new` refused with ACP `-32000` (auth required) means not signed in. |
| `goose` | ``run `goose configure` in a terminal`` | `models` starts `goose acp`: `session/new` refused with `Failed to resolve provider and model` means not signed in. |
| `gemini` | ``run `gemini` in a terminal and choose how to sign in`` | `gemini models list` lists signed in or not, so the driver reads what Gemini reads first: `security.auth.selectedType`, else the type that `GOOGLE_GENAI_USE_GCA`, `GOOGLE_GENAI_USE_VERTEXAI` or `GEMINI_API_KEY` picks. No type: not signed in. Google sign-in: `oauth_creds.json` there, else not told (the Keychain). Key: `GEMINI_API_KEY` there, else not told (the Keychain). |
| `antigravity` | ``run `agy` in a terminal and sign in with Google, or set `GEMINI_API_KEY` `` | `agy models`. Exit 0 with a list: signed in. `Please sign in` or `environment variable is not set` on standard error: not signed in. A Google sign-in lives in the system keyring, so the command is what tells. |
| `openclaw` | ``run `openclaw onboard` in a terminal`` | `openclaw models status --json --check`. Exit 0 or 2 (a sign-in expires soon): signed in. Exit 1 (`auth.missingProvidersInUse` names the providers): not signed in. |
| `droid` | ``run `droid` in a terminal and sign in, or set `FACTORY_API_KEY`; a custom model of your own runs without a Factory account`` | not told: a custom model in Droid's settings runs turns with no account, so the plan asks for no sign-in and only notes it |
| `crush` | ``run `crush` in a terminal and pick a provider`` | `crush models` on a terminal lists a provider it cannot run as `(not configured)`. No other provider: not signed in. |

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
| `copilot` | `copilot-sign-in`, `github-token` (`copilot login --with-token`) | GitHub's device page (`copilot login --device-code`), with its code in the step's label |
| `omp` | one per provider below, `api-key` | the link that `omp login <provider>` prints. A code that it shows goes in the step's label. |
| `pi` | `api-key` | none |
| `grok` | `grok-sign-in`, `api-key` (xAI, kept as `XAI_API_KEY`) | `grok login --device-auth`'s page (`accounts.x.ai/oauth2/device?user_code=…`), with its code in the step's label. A key counts where `grok models` with it, in an empty `GROK_HOME`, fetched xAI's models. |
| `amp` | `amp`, `api-key` (Amp) | the link that `amp login` prints after "Open this URL in your browser to authorize:". |
| `openclaw` | `openclaw-sign-in`, `api-key` (OpenRouter, kept as `OPENROUTER_API_KEY`) | none. `openclaw models auth login` and `openclaw onboard` pick a provider interactively, so the person runs one on this host's terminal and the driver checks `models status --check` again. A key counts where `models status --check` with it exits 0 or 2. |

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
  `pick` and `stop` in its driver. `claude-code`, `codex`, `copilot`, `cline`, `hermes`, `grok` and `antigravity` do this. omp, pi, Kilo,
  OpenCode, Amp and Gemini CLI have no door of their own, so their door is `plugin`. This
  repository keeps Steery's plugin for omp and pi in `pi-family/plugin/`, Kilo's in
  `acp-family/plugin/kilo.ts`, OpenCode's in `acp-family/plugin/opencode.ts` and Amp's in
  `amp/plugin/steery.ts`, and `setup/plan` installs it. Gemini CLI loads no plugin of
  Steery's: its turn process speaks the door itself (`acp-family/src/turn/door.rs`). The
  plugin speaks the plugin door, version 1 (§4.4), to the runtime itself. Thus the driver has
  no live methods. The runtime uses the live methods only on conversations that Steery
  started. The runtime never steers a conversation that a person started.

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
| `copilot` | `mcp-config.json` `mcpServers`, installed plugins' servers, built-in servers (`copilot mcp list --json`). `disabledMcpServers` in `settings.json` off. | not read: a folder's `.mcp.json`, `.github/mcp.json`, `.vscode/mcp.json` | not with `cwd` |
| `opencode` | the system folder's, then the config folder's `opencode.json`, `opencode.jsonc` (comments and trailing commas read), then the file `OPENCODE_CONFIG` names. `mcp.servers` (OpenCode 2) over `mcp.<name>` (OpenCode 1's shape). `enabled: false` off. | each folder's `opencode.json`, `opencode.jsonc`, from `/` down to it; then each folder's `.opencode/` ones, in the same order | not with managed preferences, `OPENCODE_CONFIG_CONTENT`, a `plugin` or `plugins` list, or a module in a config folder's `plugin/` or `plugins/` |
| `cursor` | `~/.cursor/mcp.json`, whatever `CURSOR_CONFIG_DIR` says | `.cursor/mcp.json`, listed before its person approves it in Cursor | never: plugins, team servers |
| `amp` | `amp mcp list --json` run at `/` | the same, run in the folder (it adds `.amp/settings.json`) | never: skills, plugins and toolboxes bring servers |
| `cline` | Cline's MCP settings file: `CLINE_MCP_SETTINGS_PATH`, else `<CLINE_DIR or ~/.cline>/data/settings/cline_mcp_settings.json`. `mcpServers.<name>`, nested `transport` or flat shape. `disabled: true` off. | the same file: Cline reads none per folder | not when the file is not JSON |
| `goose` | `/etc/goose/config.yaml`, the files `GOOSE_ADDITIONAL_CONFIG_FILES` names, then `<GOOSE_PATH_ROOT>/config` or `<XDG_CONFIG_HOME or ~/.config>/goose`'s `config.yaml`, a later file's entry over an earlier's. `extensions.<key>` of type `stdio`, `streamable_http` or `builtin`; `enabled: false` off. | the same files: Goose reads none per folder | never: plugins and recipes bring extensions |
| `gemini` | `mcpServers` of the system defaults, `settings.json`, the system settings, and each extension's `gemini-extension.json`. `mcp.allowed`, `mcp.excluded` and `mcp-server-enablement.json` turn servers off. | `.gemini/settings.json` where `trustedFolders.json` trusts the folder. A folder not trusted lists none. | never: an administrator's remote settings |
| `qwen` | `mcpServers` of the system defaults, `settings.json`, the system settings, `mcp.serverCommand` (named `mcp`), and each extension that `extension-enablement.json` leaves on: its `qwen-extension.json`, else its `.mcp.json`. `mcp.allowed` and `mcp.excluded` turn servers off. | `.qwen/settings.json` where the folder is trusted (`trustedFolders.json`, or folder trust off), and `.mcp.json`. These are listed only where `mcpApprovals.json` approves them under their config's hash, or `tools.approvalMode` is `yolo`. | never: servers on Qwen's command line or added in a running conversation |
| `antigravity` | `~/.gemini/config/mcp_config.json` `mcpServers`, then each plugin's `mcp_config.json` in `~/.gemini/config/plugins/<plugin>/`, named `<plugin>_<server>`. `disabled: true` off; a plugin off where `config.json` has `plugins.<plugin>.enabled: false`. | `.agents/mcp_config.json` and `.agents/plugins/<plugin>/mcp_config.json` in the folder. The person's server wins a name both give. | never: `--add-dir` widens the workspace, a plugin installed while Antigravity runs |
| `openclaw` | `mcp.servers` of `<state>/openclaw.json` (comments and trailing commas read). `enabled: false` off. A server with a `command` runs it, so it keeps no URL. | the same file: OpenClaw reads none per folder | never: enabled plugins bring servers |
| `droid` | `~/.factory/mcp.json` `mcpServers`. `disabled: true` off. | `.factory/mcp.json` in the folder and in each folder above it, the nearest ranked first. Droid loads one definition of a name given at several levels; its documentation does not say which. | never: servers its organization manages win and are not readable |
| `crush` | `/etc/crush/crush.json`, then `<CRUSH_GLOBAL_CONFIG or XDG_CONFIG_HOME/crush or ~/.config/crush>/crush.json`, then the `crush.json` of Crush's data folder. `mcp.<name>`, a later file's fields over an earlier's. `disabled: true` off. | each folder's `crush.json`, `.crush.json`, from the project's git root down to it, then `.crush/crush.json`. The store's `mcp_disabled_servers` and `mcp_enabled_servers` over every file. | not where a `crushrc` that Crush reads exists: it is a shell script |

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
  session, and the Claude Code driver never allows or blocks Cursor's tools. The `cursor`
  driver reads Cursor's turns from its own hooks, in `~/.cursor/hooks.json`.
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

## Copilot: `acp-family/src/copilot/`

One binary, `steery-driver-copilot`, with no settings, at live. It speaks to GitHub Copilot
CLI only through what Copilot publishes:

- its conversation files under `session-state/`
- `hooks/*.json`, `mcp-config.json` and `skills/`
- `copilot --headless --stdio` (Copilot's own server: JSON-RPC with `Content-Length` frames)
- the same server of a Copilot window started with `--ui-server`, on its loopback port
- `copilot -p` with `--output-format json` (a turn), `copilot -s` (a one-shot read),
  `copilot --resume=<id>`, `copilot mcp list --json` and `copilot login`

Copilot's home is `COPILOT_HOME`, else `~/.copilot`.

Copilot's own server is a module of this crate, not a crate of its own. Only Copilot speaks
it, and it is one JSON-RPC client of about 170 lines (`rpc.rs`). Drive asks it
`auth.getStatus`, `models.list`, `sessions.fork` and `sessions.delete`; Live asks a window's.

**Needs** (protocol 5). When `auth.getStatus` says `isAuthenticated: false`, the plan asks
`sign-in` in two ways:

1. `copilot-sign-in`: `copilot login --device-code`. Its page and one-time code are the step.
2. `github-token`: a GitHub token. `copilot login --with-token` reads it on standard input,
   and Copilot keeps it.

- **Conversations** (`sessions/list`, `sessions/locate`, `sessions/read`). The driver reads
  `session-state/<id>/events.jsonl`: one event on each line, each naming the event before it
  in `parentId`. `workspace.yaml` beside it gives the folder and the times. A resume
  (`--resume=<id>`) appends to the same file. The driver does not list a run whose
  `client_name` is `sdk`: a program started it through Copilot's SDK. The events are turns
  and how they ended (`abort`: interrupted), the person's and the agent's words, reasoning,
  calls and their results, each response's tokens, compactions, models and sub-agents.
- **Setup** (`setup/plan`). The plan has these entries:
  - The MCP entry in `mcp-config.json` (`mcpServers.steery`).
  - Steery's skill, `skills/steery/SKILL.md`.
  - Steery's own hooks file, `hooks/steery.json`. Copilot loads every file in `hooks/`, so the
    person's hooks stay where they are. Each hook runs the runtime's hook line with `|| true`:
    Copilot reads a non-zero exit at `preToolUse` as a denied call.
- **Hooks** (`hook/read`, `hook/answer`), in Copilot's camelCase form: `sessionStart`,
  `userPromptSubmitted`, `preToolUse`, `postToolUse`, `postToolUseFailure`,
  `permissionRequest`, `notification`, `agentStop`, `sessionEnd`, `subagentStart`.
  - `sessionStart` gives Steery's paragraph as `additionalContext`, the only output Copilot
    reads there. This is how the agent learns its conversation's id.
  - `preToolUse` answers `permissionDecision` (`allow`, `deny`, `ask`).
  - `permissionRequest` answers `behavior` (`allow`, `deny`). Its document names no call id.
  - Copilot drops the output of `userPromptSubmitted`, so Steery only reads it.
- **Drive** (`held`, `idle`, `stop`, `receipt/*`, `models`, `start`, `turn/read`, `reader`,
  `terminal`).
  - `held`: every Copilot that opens a conversation writes `inuse.<pid>.lock` in its folder.
    A lock counts while its process runs and started before the lock: a killed Copilot
    leaves its lock behind. Else the runtime looks.
  - `idle`: the newest turn of `events.jsonl`, with its model, where `assistant.turn_end`
    ended it. An open turn gives no sample: a run that died leaves its turn open.
  - `models`: the server's `models.list`, with each model's `supportedReasoningEfforts` as
    its thinking. `auto` is listed first, but a run that names no model takes the model after
    it, so the driver lists that one first.
  - `start`: this binary in its turn mode, which becomes
    `copilot --output-format json --no-ask-user [--model m] [--reasoning-effort t] [--add-dir r]`
    with the words on standard input.
    - A continue adds `--resume=<id>`. Copilot names a resumed conversation only in its last
      line, so the turn mode first writes `{"steery": {"named": <id>}}`. A continue is refused
      `no-place` where its folder is gone.
    - A fork asks the server's `sessions.fork` for a copy of the history, then resumes the copy.
    - A worktree and a hand-over are refused `selection-unsupported`.
    - The turn runs without `--allow-all-tools`: Copilot with no terminal denies each call that
      its own rules and Steery's `permissionRequest` hook do not allow.
  - `turn/read`: Copilot's lines are the events of `events.jsonl`, with the same ids, then a
    `result` with `exitCode`. `took` is the `user.message` that carries the delivery marker.
    Standard error gives `model-missing`, `not-found`, `selection-unsupported`, `start-failed`.
  - `stop`: Copilot stops a turn gracefully only for the server client that runs it
    (`session.abort`). The runtime's signals end every other turn.
  - `receipt/*`: no door, so no mark.
  - `reader`: `copilot -s --no-ask-user --no-custom-instructions --disable-builtin-mcps`, with
    `--excluded-tools` for every source, or only `view`, `grep`, `glob`. The reader runs under a
    conversation id of its own (`--session-id`), and `sessions.delete` then removes it.
  - `terminal`: `cd '<cwd>' && copilot --ui-server --resume=<id>`, with `COPILOT_HOME` where
    it is set. The window it opens has a door.
- **Live** (`held`, `say`, `pick`, `stop`, `session/changed`), door `picks`. Copilot's door
  is a window started with `copilot --ui-server`. Its server listens on a loopback port of its
  process, and `session.getForeground` names the conversation it shows.
  - `held`: a live lock's process with a listening port whose window shows the
    conversation. Its stop is `owner`, with a token of the window and the turn's start.
  - `say`: `session.send` with the say's id as `clientTurnId`. `queue` is `enqueue`; `steer`
    is `immediate`, which goes in at the running turn's next model call. `immediate` is
    refused `harness-busy` while a turn runs or words wait.
  - `pick`: `session.model.switchTo`, or `setReasoningEffort` alone. Copilot takes any model
    name and fails at the next turn, so the driver checks the pick against `models.list`.
  - `stop`: `session.abort`, where the token still names the turn that runs.
  - `session/changed`: the window's `session.eventLog.read`, waiting for new events. Copilot
    logs no `session.idle`: an `abort`, a new turn's `user.message`, or
    `session.metadata.isProcessing` false ends a turn. While a turn runs, the driver asks
    `isProcessing` each second.
  - A window that was not started with `--ui-server` has no door: `say` is refused `no-door`.
    `setup/plan` notes this. Copilot 1.0.109 accepts `--ui-server` but `--help` does not list
    it, and no setting turns it on.

```sh
cd acp-family && cargo build
steery programs test target/debug/steery-driver-copilot
cargo test
```

## Cursor: `acp-family/src/cursor/`

One binary, `steery-driver-cursor`, from release 2026.7.1. Cursor keeps each chat in a SQLite
file, `store.db`, under `chats/<md5 of its folder>/<id>/` or `acp-sessions/<id>/`.

- **Drive** (`held`, `idle`, `stop`, `receipt/*`, `models`, `start`, `turn/read`,
  `terminal`). Acts `new` and `continue`. No fork, and no words while a turn runs.
  - `held`: the process that holds SQLite's lock on the chat's `store.db`. Cursor holds it
    while the chat is open. Else the runtime looks.
  - `idle`: how the chat's last turn ended in `store.db`. It stands while nobody holds it.
  - `models`: each `<id> - <name>` line of `cursor-agent models`, the default first. Cursor
    has no thinking setting of its own: a level is part of a model's id.
  - `start`: `cursor-agent -p --output-format stream-json [--resume <id>] [--model m]
    [--add-dir r]`, with the words on standard input.
    - Cursor does not refuse `--resume` of an id it does not know: it starts a new chat
      under it. So a continue of a chat that Cursor does not keep is refused `not-found`.
    - A continue is refused `no-place` where its folder is gone.
    - `--model` also makes the model Cursor's default: Cursor writes it into
      `cli-config.json`. A continue keeps that model (`keeps.model`).
    - A thinking pick, a worktree, a fork and a hand-over are refused
      `selection-unsupported`.
    - The turn runs without `--force`. Steery's `preToolUse` hook answers each call.
  - `turn/read`: the first line, `system` `init`, names the chat, new or resumed. The `user`
    line is the words Cursor took. The last line, `result`, ends the turn. Cursor writes a
    refusal on standard error and exits 1: `ActionRequiredError: ...` (a usage limit, a
    model the plan leaves out) and `Error: ...` (signed out) end the turn `failed` with
    that line, refused `refused`. `Workspace Trust Required` is refused `untrusted`.
  - `stop`: Cursor has no way to end a turn from outside its process. The runtime's signals
    end the turn. They do not reach Cursor's `worker-server`, which outlives every turn.
  - `receipt/*`: the mark is the number of the chat's turns. A receipt is the one person's
    words after it that carry the delivery marker.
  - `terminal`: `cd '<cwd>' && cursor-agent --resume <id>`.

```sh
cd acp-family && cargo build
steery programs test target/debug/steery-driver-cursor
cargo test --test cursor
```

## Cline: `acp-family/src/cline/`

One binary, `steery-driver-cline`, from release 3.0.74. Cline keeps its conversations in
`<CLINE_DIR or ~/.cline>/data/db/sessions.db` (table `sessions`), with each one's messages in
`data/sessions/<id>/<id>.messages.json`. The driver starts Cline's turns through its own ACP
server (`cline --acp`).

- **Live** (`held`, `say`, `pick`, `stop`, `session/changed`), door `picks`. Cline's door is
  its hub: a websocket server on a loopback port that `cline hub ensure` starts. The lock file
  `<CLINE_DIR>/data/locks/hub/production.json` names its `url` and `authToken`.
  - A `cline` window joins the hub only where one already runs. In `--yolo` or `--sandbox`
    it never joins. Such a window runs its turns itself and has no door: `say` is refused
    `no-door`. `setup/plan` notes this, and `terminal` runs `cline hub ensure` first.
  - `held`: a `cli` client of the hub that takes part in the conversation. Its stop is
    `owner`, with a token of the hub's pid and the turn's start. Else Drive's `held`.
  - `say`: `session.send_input`. `queue` is delivery `queue`; `steer` is `steer`, which goes
    in at the running turn's next model call. `immediate` is refused `harness-busy` while a
    turn runs or words wait. The hub's `session.pending_prompts` events match the words to
    the say's id.
  - `pick`: `session.update_connection` with `modelId`. The hub takes any name, so the driver
    checks the pick against `models`. Cline lists no thinking levels over ACP, so a thinking
    pick is refused `selection-unsupported`.
  - `stop`: `run.abort`, where the token still names the turn that runs. The proof is the
    conversation going idle.
  - `session/changed`: the hub's event stream (`stream.subscribe` for the conversation).
    `run.started` and `session.updated` status say when a turn starts and ends;
    `tool.started` and `tool.finished` say its calls.
  - The window's footer keeps the model it opened with after a `pick`. The next turn runs on
    the picked model.

```sh
cd acp-family && cargo build
steery programs test target/debug/steery-driver-cline
cargo test
```

## Grok Build: `acp-family/src/grok/`

One binary, `steery-driver-grok`, from release 1.0.77. Grok keeps its conversations in
`<GROK_HOME or ~/.grok>/sessions/<encoded folder>/<id>/updates.jsonl`. The driver starts
Grok's turns headless (`grok -p … --output-format streaming-messages-json`).

- **Live** (`held`, `say`, `pick`, `stop`, `session/changed`), door `picks`. Grok's door is
  its leader: `grok agent leader` on the Unix socket `<root>/leader.sock` (`GROK_LEADER_SOCKET`
  overrides it). Each frame is a 4-byte big-endian length and JSON; ACP rides in
  `{"type": "acp"}` after a `register`.
  - A TUI joins the leader only in leader mode: `[cli] use_leader = true`, or `--leader`.
    Leader mode is off by default, and a sandbox profile turns it off. Such a window has no
    door: `say` is refused `no-door`. `terminal` opens `grok --leader --resume <id>`, and
    `setup/plan` notes this to the person, without reading the config.
  - `held`: a conversation that `active_sessions.json` names and the leader keeps resident.
    Its stop is `owner`, with the running prompt's id as the token. Else Drive's `held`.
  - The driver follows a conversation with `session/resume`, which attaches it to the
    leader's broadcasts with no replay.
  - `say`: `session/prompt`; the leader queues a prompt that comes while a turn runs.
    `steer` while a turn runs is `_x.ai/interject`, which goes in at the turn's next safe
    point. `immediate` is refused `harness-busy` while a turn runs or words wait.
  - `pick`: `session/set_model` with `_meta.reasoningEffort`. A thinking level Grok does not
    list is refused `selection-unsupported`.
  - `stop`: `session/cancel`, where the token still names the turn that runs. The proof is
    the conversation going idle.
  - `session/changed`: the leader's `session/update` broadcasts (`tool_call`,
    `tool_call_update`, `turn_completed`) and `_x.ai/sessions/changed` roster rows for the
    model and thinking.

```sh
cd acp-family && cargo build
steery programs test target/debug/steery-driver-grok
cargo test
```

## Qwen Code: `acp-family/src/qwen/`

One binary, `steery-driver-qwen`, at observe and drive, from release 0.27.2. Qwen keeps
everything under its root, `QWEN_HOME` or `~/.qwen`. Each conversation is one JSON-lines chat
file, `projects/<folder>/chats/<id>.jsonl` (the folder is the conversation's `cwd` with each
character that is not a letter or a digit as `-`), with `<id>.title.json` beside it.

- **Drive** (`held`, `idle`, `stop`, `models`, `start`, `turn/read`, `reader`, `terminal`).
  Acts `new`, `continue` and `fork`. No words while a turn runs.
  - `held`: Qwen keeps no lock and no pid. The runtime looks for the processes that hold
    or name the chat file. `qwen --acp` serves every conversation its client opens, so it
    is named beyond, not stopped.
  - `idle`: how the last turn in the chat file's end ended. It stands while nobody holds it.
  - `models`: `session/new`'s `models.availableModels` of a `qwen --acp` that is never
    prompted, the current one first. Qwen has no thinking level a run can pick.
  - `start`: `qwen -o stream-json --approval-mode yolo [-r <id> [--fork-session]] [-m m]
    [--add-dir r]`, with the words on standard input, which Qwen reads as the prompt.
    - `yolo` runs every call unasked, so Steery's `PreToolUse` hook is the turn's one gate.
      `start` is refused `refused` where no settings scope that Qwen loads in the folder
      holds Steery's guarded hook, or `disableAllHooks` turns hooks off.
    - The guard: where the hook fails in a turn Steery started (`STEERY_STARTED` names the
      pid of Qwen's parent), the call is blocked (exit 2). In the person's own `qwen` it
      lets the call go. Its timeout is the longest Qwen waits, as a timed-out hook lets the
      call run.
    - A continue or a fork of a conversation Qwen does not keep here is refused
      `not-found`. A model `models` does not list is refused `model-missing`. A thinking
      pick, a worktree and a hand-over are refused `selection-unsupported`.
  - `turn/read`: `system` `init` names the conversation (a fork's new id). A `tool_use` or
    `tool_result` is a call that began or ended. `result` ends the turn: `success`
    finished, else failed with its error; an `API Error: 404 model ... not found` is
    refused `model-missing`. On standard error, `No saved session found with ID` is refused
    `not-found` and an auth error `refused`.
  - `stop`: Qwen has no way to end a turn from outside its process. The runtime's signal
    ends it: Qwen exits 130 with no `result`, and the turn reads interrupted.
  - `reader`: `qwen -o text --chat-recording=false --approval-mode plan`, its tools held to
    `read_file`, `grep_search`, `glob` or none, and no MCP server.
  - `terminal`: `cd '<cwd>' && qwen -r <id>`, with `QWEN_HOME` where the root is not
    `~/.qwen`.

Ran (Qwen Code 0.27.2, `/opt/homebrew/bin/qwen`, ollama `qwen3:1.7b`, a temporary
`QWEN_HOME`): a `new` turn through `start` that ran `run_shell_command` past the hook and
finished; a `continue` with the hook denying, the call blocked and the turn finished; a
`fork` whose `init` named a new conversation, its chat file holding the source's records and
the source's file left as it was; a `stop` by SIGINT (exit 130, interrupted);
a missing hook binary blocking every call of a turn Steery started, and letting the calls
run where `STEERY_STARTED` names another process; `start` refused where
the settings hold no hook. The recorded turns are in `acp-family/tests/qwen/turns`.

```sh
cd acp-family && cargo build
steery programs test target/debug/steery-driver-qwen
cargo test --test qwen
```

## Antigravity: `acp-family/src/antigravity/`

One binary, `steery-driver-antigravity`, from release 1.5.5. The driver starts Antigravity's
turns through its own ACP server (`agy acp`).

- **Live** (`held`, `say`, `stop`, `session/changed`), door `keeps`. Antigravity's door is
  the language server inside each `agy` window. It speaks Connect (HTTP/1.1, JSON) on a
  loopback port of the window's process. It takes a request only with the CSRF token that the
  window read from `ANTIGRAVITY_SERVER_CSRF_TOKEN` as it started. Without that variable the
  token is random and never leaves the window.
  - A window that started without the variable has no door: `say` is refused `no-door`.
    `setup/plan` notes this. In `terminal` the shell makes a fresh token as the command runs,
    so the window it opens has a door and no history keeps the token. The driver reads the
    token from the window process's environment.
  - `held`: the process that holds the conversation's presence lock, where a port of it
    answers `Heartbeat` with that token. Its stop is `owner`, with a token of the window and
    the turn's start. Else Drive's `held`.
  - `say`: `SendUserCascadeMessage` with the say's id as `clientMessageId`. `queue` is
    `MESSAGE_DELIVERY_STRATEGY_WHEN_IDLE`, which goes in at once where nothing runs. `steer`
    is `MESSAGE_DELIVERY_STRATEGY_NEXT_INVOCATION` while a turn runs. `immediate` is refused
    `harness-busy` while a turn runs or words wait.
  - `pick` is refused `selection-unsupported`. The server takes a model only together with
    words, and the person's next turn goes back to the model in the window's footer.
  - `stop`: `CancelCascadeInvocation`, where the token still names the turn that runs.
    Steered words that wait stay waiting, and go in first at the next turn.
  - `session/changed`: the window's `StreamAgentStateUpdates`. Its first message is the
    whole state, then what changed: `status` (`CASCADE_RUN_STATUS_IDLE` ends a turn),
    `pendingAgentMessagesUpdate`, and the steps of `mainTrajectoryUpdate` for the says that
    went in and the calls that run.

```sh
cd acp-family && cargo build
steery programs test target/debug/steery-driver-antigravity
cargo test
```

## OpenClaw: `acp-family/src/openclaw/`

One binary, `steery-driver-openclaw`, at observe and drive, from release 2026.11.3. OpenClaw keeps
everything in its state folder, `OPENCLAW_STATE_DIR` or `~/.openclaw`. Each agent's
conversations are in its SQLite store, `agents/<agent>/agent/openclaw-agent.sqlite`, which the
driver opens read-only.

- **Conversations.** A conversation is a window of `session_windows`, by its `session_id`;
  windows that `previous_session_id` links by `compaction`, `rollover` or `recovery` read as
  one. Its records are the rows of `transcript_events`, JSON or zstd-packed, each naming its
  parent; the read walks the path from the active leaf, through `leaf` records. A sub-agent
  is a `session_nodes` row whose `spawned_by` names the parent; its task is its first
  message with OpenClaw's internal context blocks left out. A heartbeat poll or a retry
  prompt begins a turn with no words of the person's. `run-failed-before-reply` fails the
  turn. An in-place compaction is `compacted` with its token counts and summary.
- **Setup.** Two entries:
  - Steery's MCP server at `/mcp/servers/steery` of `openclaw.json`, with `transport:
    streamable-http` (a URL server without it speaks SSE) and `requestTimeoutMs`. `openclaw
    acp` refuses the servers that an ACP client forwards, so Steery's server sits in the
    person's own `mcp.servers`. `openclaw mcp set` refuses while the gateway runs. A server
    written into the file directly works: `openclaw mcp status --json` then shows it `ok`.
    OpenClaw parses JSON5, and the runtime writes plain JSON. Thus where the file is not
    plain JSON, the plan notes it.
  - Steery's skill in `skills/steery/SKILL.md`, the managed skills folder, which every agent
    reads.
  - Steery's hook command line in `steery/hook` of the state folder, which OpenClaw does not
    read. OpenClaw's own hooks are JavaScript modules, not commands, so only a turn that this
    driver runs calls Steery's hook.
- **Needs** (protocol 5): `sign-in`, where `openclaw models status --json --check` exits 1.
  See Signing in.
- **`models`**: `openclaw models list --json`, the models of its configured providers that
  are `available`, with the one tagged `default` first. Each key `<provider>/<model>` is split
  at its first `/`.
- **Drive** (`start`, `held`, `idle`, `receipt/mark`, `receipt/find`, `terminal`,
  `turn/read`, `hook/read`, `hook/answer`). A turn is this driver's binary in its turn mode,
  speaking ACP to `openclaw acp`, a bridge onto the gateway that runs every turn.
  - `new`: `session/new` with `mcpServers []`; the bridge names the session
    `<agent>:acp-bridge:<id>`. `continue`: `session/resume` by that key. `fork` is refused
    `selection-unsupported`: the bridge has none.
  - Steery names a conversation by its first window's id. `turn/read` maps the ACP id back to
    it. A bridge key sets only `parent_session_key`, so only `spawned_by` marks a sub-agent.
  - The model: `openclaw gateway call sessions.patch` on the key before the turn, as the
    bridge has no model option. A model the last `models` lacks is refused. The thinking
    level is the session's `thought_level` option.
  - Words told mid-turn go in as the next prompt, after the running one ends: a second
    prompt cancels the running one.
  - A permission is the gateway's exec approval, relayed with the call's `rawInput`.
    `hook/read` names it as the `exec` call. It reaches ACP only where exec `ask` is on.
  - `held`: the newest window of a key that a run writes now (`activeWriterRunId`).
  - `terminal`: `openclaw tui --session <key>` in the conversation's folder.

Ran (OpenClaw 2026.11.3, `/opt/homebrew/bin/openclaw`, ollama `qwen3:1.7b`): five real
conversations of `openclaw agent` (an `exec` call, two failed turns, one zstd record, one
`openclaw sessions compact`) and a heartbeat, listed and read from `~/.openclaw`;
`setup/plan`, `mcp/list` and `models` against `~/.openclaw` (468 models, `signed_in: true`);
and against an empty `OPENCLAW_STATE_DIR` with no key, where `models status --check` exits 1
and the plan asks for `sign-in`. A real sub-agent, spawned by `sessions_spawn` through the
gateway's `tools.invoke` (with `gateway.tools.allow` lifting its default deny for the probe),
listed under its lead, left out of the list, and read in place: its task, its answer.

Drive ran on the live gateway (`127.0.0.1:18789`, ollama `qwen3:1.7b` and `qwen3:0.6b`):
`new` (`end_turn` in 19.3 s) and three `continue` turns; a `write` tool call read back;
`receipt/find` true for its delivery, false for another; `--model ollama/qwen3:0.6b` set
by `sessions.patch`, both answers from `qwen3:0.6b`; words told on the inbox heard and
answered as the next prompt; with exec `ask: always`, an `exec` approval through the hook,
read by `hook/read`, allowed once, then the turn-end hook; `held` and `idle` going `running`
then `finished` during a turn; `steery programs test` all `ok`.

Not verified: window chains, forks and `leaf` records (none in a real store: `/new` and an
idle `session.reset` kept the same window); `needs/give` with a real OpenRouter key; the
installer on a host without OpenClaw; `~/.npm-global/bin` and `~/.local/bin` as install
folders (read from `install.sh`, not seen installed); a reject at the hook; live (the
gateway's `chat.send` and `chat.abort` need an operator token, not built).

```sh
cd acp-family && cargo build
steery programs test target/debug/steery-driver-openclaw
cargo test
```

## Gemini CLI: `acp-family/src/gemini/`

One binary, `steery-driver-gemini`, with no settings, at observe, drive and live, from release
0.67.0. Gemini keeps each conversation in `<GEMINI_CLI_HOME or ~>/.gemini/tmp/<project>/chats/`,
one append-only JSON-lines file each (`session-<time>-<id8>.jsonl`).

- **Drive** (`start`, `held`, `idle`, `receipt/*`, `models`, `turn/read`, `terminal`). Acts
  `new` and `continue`; no fork, no hand-over, no thinking level. A turn is this driver's
  binary in its turn mode, speaking ACP to `gemini --acp` in the turn's own process.
  - `held`: Gemini keeps no lock and no pid of a running turn, so the runtime looks.
  - `terminal`: `cd '<cwd>' && gemini --resume <id>`.
- **Live**, door `plugin`. Gemini loads no plugin of Steery's, so the turn process is the
  conversation's copy. The runtime keeps it past its words (`stdin: held`). It dials the door
  that `setup/plan` wrote to `<root>/steery/door`, `mode: rpc`, with its own pid.
  - `say`: a second `session/prompt` aborts Gemini's running turn
    (`packages/cli/src/acp/acpSession.ts`). Thus words to a running turn, `steer` or `queue`,
    wait in the copy (`queued_at`, `pending`) and go in together as the next prompt at its end
    (`taken_at`). `idle` words to a running turn are refused `harness-busy`.
  - `stop`: `session/cancel`; `pick`: `session/set_model`. `compact` runs nothing of
    Steery's. The copy keeps no records: a welcome that asks for entries gets `lost`.

Ran (Gemini CLI 0.67.0, `npm install -g @google/gemini-cli`, a model API on the loopback
through `GOOGLE_GEMINI_BASE_URL`): `tests/gemini.rs` runs `setup/plan`, `start` and the launch
plan. The real `gemini --acp` dials the test as the door. A `say` goes in at once; words said
while it runs wait and go in at its end; `stop` cancels a running turn; the runtime's signal
closes the door; `sessions/list` and `sessions/read` then read Gemini's own chat file.

Not verified: a signed-in Google model (no account here); a permission through the hook; a
`pick` of a model; `force` on a running turn.

```sh
cd acp-family && cargo build
steery programs test target/debug/steery-driver-gemini
PATH="<gemini's folder>:$PATH" cargo test --test gemini
```

## Droid: `acp-family/src/droid/`

One binary, `steery-driver-droid`, at observe and drive, from release 1.9.0. Droid keeps everything
in `~/.factory`. Each conversation is one JSON-lines file under `sessions/`, in one of three
layouts: `sessions/<folder>/<id>.jsonl` (the folder is the conversation's `cwd` with each `/`
as `-`), `sessions/<id>.jsonl`, and `sessions/btw/<id>.jsonl` (a side question). Beside each,
`<id>.settings.json` holds its model and the tokens it spent.

- **Conversations.** The first record, `session_start`, gives the title, the folder, and in a
  fork `parent` and `forkedAtMessageId`. A fork reads as its own conversation beside its
  source, its `parent` named. Each `message` names its parent by `parentId`; a turn runs from a
  person's prompt (a user message with `interactionMode`) to its `agent_turn_outcome`, matched
  by `turnId`. `model_provider_unreachable` and other failed outcomes fail the turn; a turn
  with no outcome whose file stood still for 60 s reads as interrupted. A resumed conversation
  appends to its own file. `llm_only` and `context-…` messages (the system reminders Droid
  sends the model) are not the person's words. A file with no person's prompt is excluded.
- **Setup.** Entries the runtime writes:
  - Steery's MCP server at `/mcpServers/steery` of `~/.factory/mcp.json`.
  - One hook per event below in `~/.factory/hooks.json`, beside the person's own. Droid runs
    the user's hooks in every folder, with no trust prompt.
  - Steery's skill in `~/.factory/skills/steery/SKILL.md`.
  - No `droid`: Factory's installer, `curl -fsSL https://app.factory.ai/cli | sh`. Older than
    1.9.0: refused, with `droid update`.
- **Hooks.** `SessionStart`, `UserPromptSubmit`, `PreToolUse` (asks), `PostToolUse`,
  `Notification`, `Stop`, `SessionEnd`. Each document comes on standard input in Claude Code's
  shape (`session_id`, `transcript_path`, `cwd`, `hook_event_name`, …); the answer is printed
  in Claude Code's shape too (`hookSpecificOutput.permissionDecision`, `additionalContext`,
  `decision: "block"`). A tool's document carries no call id. `SessionStart` appends Steery's
  lead to `CLAUDE_ENV_FILE`.
- **Knowing.** `FACTORY_SESSION_ID` in the agent's shell names the conversation.
- **Models.** Droid has no models command; `models` reads `droid exec --help`: "Available
  Models" (the default marked `(default)`, listed first), "Custom Models" from
  `~/.factory/settings.json`, and each Factory model's levels from "Model details". A custom
  model lists no levels. It watches `~/.factory/settings.json`.
- **Drive.** A turn is `droid exec -o stream-json --skip-permissions-unsafe [-s <id> | --fork
  <id>] [-m <model>] [-r <level>] --cwd <cwd>`, the words on standard input. Each line of
  standard output is one record that names the conversation by `session_id`: `system`/`init`
  (the model), the person's `message` (the words went in, under the turn id the file gives),
  `tool_call` and `tool_result` (the file's call ids), `completion` (finished) or `error`
  (failed; `Authentication failed` is `refused`). Droid refuses an unknown model
  (`Invalid model:`, `model-missing`) or level (`Unsupported reasoning effort`,
  `selection-unsupported`) on standard error before any record.
- **The gate.** Under `--auto`, Droid's own gate ends a headless turn (`Exec ended early:
  insufficient permission`) at a call its level or its denylist does not allow: `--auto low`
  at an `Execute`, `--auto high` at a denylisted command. So the turn runs with
  `--skip-permissions-unsafe`, Steery's `PreToolUse` hook is its only gate, and `start` refuses
  without that hook in `~/.factory/hooks.json`. A denied call fails and the turn goes on. Where
  Steery's hook itself fails in a turn Steery started (`STEERY_STARTED` names Droid's parent),
  the setup command exits 2: Droid blocks the call.
- **Continue.** `-s` of an id Droid does not keep opens a new conversation under that id, so
  `start` refuses an id the store lacks (`not-found`), and a gone folder (`no-place`). No
  worktree, no `hand`. `stop` is the runtime's signals; `terminal` is `cd '<cwd>' && droid
  --resume '<id>'`.

Ran (Droid 1.9.0, `~/.local/bin/droid`, no Factory account, ollama `qwen3:1.7b` as a custom
model): real `droid exec` conversations in `/tmp/droid-obs` (an `Execute` call, a resume, a
fork, a failed turn, an interrupted turn, an MCP call) listed and read from `~/.factory`;
`hook/read` over the documents Droid handed a logging hook for each event above; `setup/plan`
and `mcp/list` against `~/.factory`. For drive: `droid exec -o stream-json` turns under
`--auto low`, `--auto high` and `--skip-permissions-unsafe`, with a hook that let the call
run, denied it, and failed; a resume, a fork, a killed turn, an unknown model and level, and a Factory model
signed out. `fixtures/droid/turns` holds those records.

Not verified: sub-agents (`Task`, `SubagentStop`), compaction (`PreCompact`), a `Notification`
document (none fired in `droid exec`), the `sessions/<id>.jsonl` and `sessions/btw/` layouts
(read from the store's shape, no real file), which of several same-named MCP servers Droid
loads, the installer on a host without Droid, and any turn on a Factory model (no account).

```sh
cd acp-family && cargo build
steery programs test target/debug/steery-driver-droid
```

## Amp: `amp/`

One binary, `steery-driver-amp`, with no settings, at observe, drive and live. Amp keeps its threads
on ampcode.com, not on this host. Thus the driver reads them only through Amp's own CLI:

- `amp threads list --json` and `amp threads export <thread>`
- `amp -x --stream-json` and `amp threads continue <thread> -x --stream-json`, which the
  runtime runs
- `amp mcp list --json`
- `amp login`

It reads one file of Amp's own: `installationID` in Amp's `device-id.json`
(`~/.local/share/amp`, or `$XDG_DATA_HOME/amp` on Linux). Nothing else of that folder: it also
holds Amp's accounts and secrets. Amp's settings are `$XDG_CONFIG_HOME/amp/settings.json`, else
`~/.config/amp/settings.json`.

**Needs** (protocol 5). When `amp threads list` says Amp is not signed in (see Signing in), the
plan asks `sign-in` in two ways:

1. `amp`: `amp login`, on a terminal of the driver's. Its printed link is the step
   `amp-return`. Where it returns to Amp's loopback listener, the address fills the step. Else
   the person pastes Amp's code, which the driver types into the login's prompt.
2. `api-key`: an Amp API key (`sgamp_…`, made at ampcode.com/settings/security). The driver
   checks it with `amp threads list`, and gives it back as `AMP_API_KEY`. The runtime keeps it
   sealed. A key in the environment wins over Amp's saved accounts.

- **Conversations** (`sessions/list`, `sessions/locate`, `sessions/read`).
  - `sessions/list` asks for the threads of this host's installation:
    `amp threads list --json --include-archived --limit 500 --installation-id <id>`. A listing
    that fills 500 is not complete. The list does not say how a thread ran. Thus the driver
    exports each thread that is new or changed since it last listed it: at most 50 in one call,
    8 at a time, within 10 s. A thread left unexported waits for the next call, and the listing
    is not complete. A thread that `amp -x` ran (client `CLI Execute Mode`) had nobody at it:
    it is `excluded`.
  - A session names no transcript, because this host has none. Its folder is the thread's
    first tree.
  - `sessions/read` exports the thread whole within 8 s, under the method's 10 s bound. Amp says
    "does not exist" for a thread that is gone or another account's: `not-found`. The driver
    refuses an export larger than 64 MiB.
  - The events are these: turns and how they ended (`end_turn`, `cancelled`, `error`), the
    person's words and pictures, the agent's words and thinking, calls and their results, each
    response's tokens, and the model. A message that Amp still writes holds the cursor back.
    A person can edit an earlier message, which cuts the thread there. Then the cursor's last
    message is gone, and the read starts again (`reset`).
  - `sessions/fold` is refused `selection-unsupported`: Steery's plugin sends no records.
- **Setup** (`setup/plan`). The plan has three entries: the MCP entry in Amp's settings
  (`amp.mcpServers.steery`, its URL alone), the skill in Amp's own skills folder
  (`skills/steery/SKILL.md` under Amp's settings folder), and Steery's plugin in Amp's system
  plugins folder (`plugins/steery.ts` there), the door's address filled in. Amp names no thread
  in an MCP call's `_meta`, so the entry sends no `Steery-*` header.
- **MCP servers** (`mcp/list`): see MCP servers.
- **Drive** (`start`, `turn/read`). A turn is `amp -x --stream-json`, the words on standard
  input. A new thread adds `--no-archive-after-execute`, as `amp -x` archives its thread
  otherwise. A continue is `amp threads continue <thread> -x --stream-json`.
  - Amp picks no model: it runs a mode. `models` lists the built-in modes `medium` (Amp's
    default), `low`, `high`, `ultra`, passed as `-m`. `claude-code` needs a linked Claude
    subscription and is left out. No thinking pick.
  - A thread keeps the mode of its first message. A continue that asks another mode is
    refused `selection-unsupported` (`keeps.model`). Fork and hand: `selection-unsupported`.
  - `turn/read`: `system/init` names the thread before Amp takes the words. The `user` line
    that holds the delivery marker is `took`. `result` ends the turn, failed with Amp's
    `error` where it is one. Standard error's `Error:` line: `harness-busy` (the thread is
    open in another Amp), `not-found`, `refused` (signed out, out of credits), else
    `start-failed`. A line gives no event: the runtime reads the thread.
  - `held`: Amp keeps no registry of who has a thread open, so the runtime looks at stray
    `amp` processes. `idle` exports the thread and samples its last ended turn. `stop` has no
    graceful step; with `say`, `other-turn`. No `reader`: `amp -x` saves every run as a
    thread and cannot run without its tools. `terminal`: `amp threads continue <thread>` in
    the folder, and the thread's page on ampcode.com as its app.
- **Live**, door `plugin`. Amp runs each plugin as a child of the `amp` process, TUI or `-x`.
  For each root thread that starts there, the plugin dials the door (§4.4). Its `hello` names
  the thread, the `amp` process's pid, and the mode: `json` for a turn Steery started
  (`STEERY_STARTED` names the `amp` process's parent), else `tui`, or `print` for a person's
  `amp -x`. A thread with a parent thread is Amp's sub-agent: it opens no door. The door carries
  no entries and no `live` frames: Amp gives a plugin no text while a message forms.
  - `say`: `appendUserMessage`, with `steer` while a turn runs. `queue` words wait for the turn's
    end. `agent.start` takes them. A one-shot run takes words only during its own turn.
  - `pick`: `selection-unsupported` (a thread keeps its mode).
  - `stop`: Amp's `cancel`, where the turn that runs is one that its words began; else
    `other-turn`. A stop that names no say parks the thread.
  - `settle` at `agent.end`: the runtime's words continue the thread (`action: continue`).
  - `compact`: declined; the context starts at the thread's first message.

What ran live, on an account with no credits (Amp 0.0.1794225648 and 0.0.1794484845):

- A new turn through the driver's plan: named, took, ended `failed` "Out of Credits",
  `refused`. `sessions/read` then gave the person's words.
- A continue in another mode: refused before anything ran. `terminal`: the command and URL.
- `amp threads continue <thread> -x` with no credits appends the words, prints nothing and
  exits 1 (Amp's log: `ReferenceError: Cannot access 'ae' before initialization`). Stdin,
  argument and `--stream-json-input` alike. A continue that reaches the model is not checked.
- Steery's plugin as a project plugin under `amp -x --stream-json` (0.0.1794484845), against a
  stand-in door: `hello` (the thread's `T-…`, `mode: json`, the `amp` pid, cwd), `welcome`,
  `state`, and the turn's begin. Out of credits, Amp exits before `agent.end`, so `say`,
  `stop` and `settle` on a running turn are not checked.

How an agent in Amp learns its thread's id is not checked yet: no Amp turn has reached the
model under the driver. The skill tells the agent to send the `T-…` of its thread's address,
or `AMP_THREAD_ID` where it is set.

Build and check it, with Amp on `PATH`:

```sh
cd amp && cargo build
steery programs test target/debug/steery-driver-amp
cargo test
cd plugin && npm install && npm run check
```

## Crush: `crush/`

One binary, `steery-driver-crush`, with no settings, at live. Crush keeps each
project's sessions in that project's SQLite store, `<project>/.crush/crush.db`. The
projects are listed in `projects.json` of Crush's data folder: `$CRUSH_GLOBAL_DATA`, else
`$XDG_DATA_HOME/crush`, else `~/.local/share/crush`. Crush never lists there a project that
only its server opened, so the driver adds the server's workspaces (`GET /v1/workspaces`,
within 1 s), each store once. The driver opens each store read only.

- **Conversations** (`sessions/list`, `sessions/locate`, `sessions/read`).
  - `sessions/list` lists every session of every project, where the project is.
    A sub-agent (`parent_session_id`) is listed under its lead. A session whose id begins
    `title-` is Crush's own, which names its lead, and is never listed.
  - `sessions/read` reads a session's `messages` in `created_at` order, 200 at a time.
    The events are these: turns and how they ended, the person's words and pictures, the
    agent's words and thinking, calls and their results, a sub-agent's start, a summary, and
    the model. An `agent` call's sub-agent is the session `<message>$$<call>`.
  - A turn ends at an answer's `finish`: `end_turn` finished, `canceled` interrupted,
    `error` failed with the provider's words. Crush keeps tokens only of a session's newest
    response, so a read gives them once, at its end.
  - An answer that the model still writes has no `finish`. It holds the cursor back.
  - `sessions/fold` is refused `selection-unsupported`: Crush's door is `keeps`, not a plugin.
- **Setup** (`setup/plan`). The plan has the MCP entry in the person's `crush.json`
  (`mcp.steery`, `type: http` and its URL), the skill in Crush's own skills folder
  (`$CRUSH_SKILLS_DIR`, else `<XDG_CONFIG_HOME or ~/.config>/crush/skills`), and one hook
  entry per event, appended to `hooks.<event>` of the same `crush.json`. Crush sends no
  `_meta` with an MCP call, so the entry sends no `Steery-*` header.
- **MCP servers** (`mcp/list`): see MCP servers.
- **Drive** (`start`, `turn/read`, `stop`, `held`, `idle`, `models`, `receipt/*`,
  `terminal`, `hook/read`, `hook/answer`). Acts: new and continue; no fork, no worktree.
  - `start` runs `steery-driver-crush turn <home> <crush> run -q -v`, with `-s <id>`,
    `-m <model>` and `--reasoning-effort <level>` where given, the words on standard input.
    `crush run` prints only the answer, so the turn mode reads Crush's log (`-v`) for the
    session id, then the project's store for how the turn ended.
  - A continue runs only in the project whose store keeps the session: else `no-place`.
  - `crush run` approves every call of its session itself (`AutoApproveSession`), so
    Steery's `PreToolUse` hook is the turn's only gate. `start` is refused `refused` where
    the person's `crush.json` lacks it.
  - Hooks: `PreToolUse` (asks, 24 h), `PostToolUse` and `PostToolUseFailure` (say, 5 s).
    Words for the session reach it in the `context` of a call's result.
  - Crush runs a call whose hook fails. The `PreToolUse` entry runs the hook in a subshell
    and exits 2 where it fails in a turn Steery started: the turn mode passes
    `STEERY_STARTED` to Crush under its own pid, and the hook's `$PPID` is that pid.
  - `stop`: the turn mode gets the runtime's signal and sends Crush SIGINT, as Ctrl-C.
    Crush writes the turn `canceled`.
  - `held`: a person's window that shows the session through Crush's server holds it
    (Live, below). Else every Crush with the project open holds its store, so the runtime
    looks there.
  - `models`: `crush models` on a terminal, each `<provider>/<id>`, with labels, thinking
    and costs from Crush's `providers.json`; `models.large` first.
  - `terminal`: `cd '<project>' && crush --session <id>`.
- **Live** (`held`, `say`, `pick`, `stop`, `session/changed`). Door `keeps`: Crush's server.
  - A Crush TUI opened with `CRUSH_CLIENT_SERVER=1` runs its sessions in `crush server`,
    one per person, on `crush-<uid>.sock` in `$XDG_RUNTIME_DIR`, else the temporary folder
    (`getconf DARWIN_USER_TEMP_DIR` on a Mac), else `/tmp`. HTTP/1.1 over that socket.
  - A session is held where the server's workspace for its project shows it to a client
    (`attached_clients` above 0). A TUI without the variable runs in its own process: no door.
  - The driver follows the workspace's events (`GET /v1/workspaces/<id>/events`) and sends
    `session/changed`: the turn, the says it took, calls, permissions and questions as
    dialogs, the model of the last answer, and settled turns. The person answers a dialog in
    the window. A dialog that opened before the driver followed is not seen: the server
    lists no open permission.
  - `say` posts `/agent`. Words that steer go without a run id and Crush folds them into the
    running turn at its next step. Others carry a run id and queue as their own turn.
    `took` once Crush writes their message or lists them in its queue.
  - `pick` with a model or thinking is refused `selection-unsupported`: Crush picks the model
    for a whole workspace. With neither it reads back the session's model.
  - `stop` posts `/agent/sessions/<id>/cancel` and waits up to 8 s for the session to go idle.
    Crush drops the words that wait with it.

Crush puts no session id in a shell command's environment or an MCP call. The skill tells
the agent to run `crush session last --json` in its folder and send `meta.uuid`. A live
turn checked it: the agent's `bash` call printed its own session's id.

Build and check it, with Crush on `PATH`:

```sh
cd crush && cargo build
steery programs test target/debug/steery-driver-crush
cargo test
```

## License

MIT, see [LICENSE](LICENSE).
