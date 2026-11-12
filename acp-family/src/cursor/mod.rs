//! Steery's driver for Cursor's agent CLI (`cursor-agent`) at observe and drive: it reads
//! Cursor's chats from their SQLite stores, plans the entries that set Cursor up for Steery,
//! reads its hooks' moments, lists the MCP servers that Cursor loads, and starts, follows
//! and stops Cursor's turns ([`drive`]).

mod calls;
mod drive;
mod hooks;
mod sessions;
mod store;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::{Value, json};
use steery_driver_common::mcp::{self, Servers};
use steery_driver_common::program::{
    executable, lacks, on_path, others_note, release_within, version,
};
use steery_sdk::wire::harness::{
    Entry, Format, Found, Install, McpList, McpServers, Part, Planned, Setup,
};
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, listing, method, result};

use crate::Harness;

/// The executable Cursor's installer names it by. It also installs it as `agent`, a name
/// that other programs take too.
const EXECUTABLE: &str = "cursor-agent";

/// Cursor's own installer.
const INSTALLER: &str = "curl -fsS https://cursor.com/install | bash";

const KNOWING: &str = "Your shell's environment holds it: `CURSOR_CONVERSATION_ID`, which is \
already a UUID. It names the chat that your person is in. Read it with your shell tool when \
you send it, for example `echo $CURSOR_CONVERSATION_ID`. Do not read it from your own context. \
If the variable is empty, describe nothing.";

const TRANSCRIPT: &str = "A SQLite file. Its `blobs` table holds protobuf and JSON blobs by \
the hex of their sha256. Its `meta` row `0` is the hex of a JSON object whose \
`latestRootBlobId` names the root, which lists the JSON messages that the model reads and the \
chat's turns.";

/// The driver: the protocol it picked from `initialize`, where Cursor keeps its chats, and
/// the listings it answered, to answer the next with only what changed.
#[derive(Default)]
pub struct Driver {
    place: OnceLock<Place>,
    changes: listing::Changes,
    turns: drive::Turns,
}

struct Place {
    home: PathBuf,
    store: Result<sessions::Store, String>,
}

impl Driver {
    fn info() -> wire::HarnessInfo {
        wire::HarnessInfo {
            levels: vec![wire::Level::Observe, wire::Level::Drive],
            minimum: Some(wire::Minimum {
                release: "2026.7.1".into(),
                update: Some(format!("{EXECUTABLE} update")),
            }),
            sign_in: Some(format!("run `{EXECUTABLE} login` in a terminal")),
            knowing: KNOWING.into(),
            lines: wire::Lines::Agent,
            hosted: Default::default(),
            transcript: Some(TRANSCRIPT.into()),
            identity: wire::Identity {
                executables: vec![EXECUTABLE.into(), "agent".into()],
                package: Some("cursor-agent".into()),
                session_variable: Some("CURSOR_CONVERSATION_ID".into()),
                call_meta: None,
            },
            setup: wire::SetupFacts {
                reads_skills: true,
                cuts_tool_text: false,
                call_bound: None,
            },
            reader: None,
            facts: wire::HarnessFacts {
                by_file: false,
                resumes_in_place: true,
                forks_beside: false,
                // A continue runs on the model in `cli-config.json`, the last that Cursor
                // used anywhere: `--model` writes it there.
                keeps: wire::Keeps {
                    model: true,
                    thinking: false,
                },
                door: None,
                checks_idle: false,
                worktrees: false,
                acts: vec![harness::Act::Continue, harness::Act::New],
                touches: false,
            },
            hooks: hooks::table(),
        }
    }

    fn place(&self) -> Result<&Place, Error> {
        self.place
            .get()
            .ok_or_else(|| Error::invalid("`initialize` must come first"))
    }

    fn store(&self) -> Result<&sessions::Store, Error> {
        self.place()?
            .store
            .as_ref()
            .map_err(|why| Error::refused("refused", why.clone()))
    }
}

impl Program for Driver {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        let protocol = steery_driver_common::program::protocol(&params.protocol)
            .ok_or_else(|| Error::invalid(steery_driver_common::program::unspoken()))?;
        let _ = self.place.set(Place {
            home: params.home.clone(),
            store: root(&params.home).map(sessions::Store::new),
        });
        self.changes.picked(protocol);
        Ok(wire::Initialized {
            protocol,
            program: Harness::Cursor.program()?,
            harness: Some(Box::new(Self::info())),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        match method {
            method::SETUP_PLAN => {
                let place = self.place()?;
                result(plan(&place.home, &place.store, steery_sdk::params(params)?).await)
            }
            method::HOOK_READ => result(hooks::read(steery_sdk::params(params)?)?),
            method::HOOK_ANSWER => result(hooks::answer(steery_sdk::params(params)?)?),
            method::SESSIONS_LIST => {
                let asked: harness::List = steery_sdk::params(params)?;
                let after = asked.after.clone();
                result(
                    self.changes
                        .answer(after.as_deref(), self.store()?.list(asked)),
                )
            }
            method::SESSIONS_LOCATE => result(self.store()?.locate(steery_sdk::params(params)?)?),
            method::SESSIONS_READ => result(self.store()?.read(steery_sdk::params(params)?)?),
            method::MCP_LIST => result(mcp_list(&self.place()?.home, steery_sdk::params(params)?)),
            method::MODELS => {
                let home = &self.place()?.home;
                result(drive::models(binary(home), home, self.store()?.root()).await?)
            }
            method::START => result(drive::start(
                binary(&self.place()?.home),
                self.store()?,
                steery_sdk::params(params)?,
            )?),
            method::TURN_READ => result(self.turns.read(steery_sdk::params(params)?)),
            method::HELD => result(drive::held(self.store()?, steery_sdk::params(params)?)?),
            method::IDLE => result(drive::idle(self.store()?, steery_sdk::params(params)?)?),
            method::STOP => result(drive::stop(&steery_sdk::params(params)?)?),
            method::RECEIPT_MARK => {
                result(drive::mark(self.store()?, steery_sdk::params(params)?)?)
            }
            method::RECEIPT_FIND => {
                result(drive::find(self.store()?, steery_sdk::params(params)?)?)
            }
            method::TERMINAL => {
                result(drive::terminal(self.store()?, steery_sdk::params(params)?)?)
            }
            _ => Err(Error::Unknown),
        }
    }
}

/// Where Cursor keeps its chats: `CURSOR_CONFIG_DIR`, else `$XDG_CONFIG_HOME/cursor`, else
/// `~/.cursor`, as Cursor's own `paths.js` reads them.
fn root(home: &Path) -> Result<PathBuf, String> {
    let set = |name| std::env::var_os(name).filter(|value| !value.is_empty());
    let root = match (set("CURSOR_CONFIG_DIR"), set("XDG_CONFIG_HOME")) {
        (Some(dir), _) => PathBuf::from(dir),
        (None, Some(dir)) => PathBuf::from(dir).join("cursor"),
        (None, None) => home.join(".cursor"),
    };
    if root.is_absolute() {
        Ok(root)
    } else {
        Err(format!("{} must be an absolute path", root.display()))
    }
}

/// Where Cursor reads its person's MCP servers and skills: `~/.cursor`, whatever moves its
/// chats.
fn own(home: &Path) -> PathBuf {
    home.join(".cursor")
}

/// The folder Cursor's installer writes `cursor-agent` to, which a shell that has not read
/// its profile since may not have on `PATH`.
fn installer_folder(home: &Path) -> PathBuf {
    home.join(".local/bin")
}

/// Cursor's executable: the first on `PATH`, else the one in its installer's folder.
fn binary(home: &Path) -> Option<PathBuf> {
    on_path(EXECUTABLE).or_else(|| executable(installer_folder(home).join(EXECUTABLE)))
}

/// Steery's MCP server, as the entries name it.
const SERVER: &str = "steery";

async fn plan(home: &Path, store: &Result<sessions::Store, String>, setup: Setup) -> Planned {
    let refused = |why: String| Planned {
        found: None,
        refused: Some(why),
        entries: Vec::new(),
        notes: Vec::new(),
        install: None,
        needs: Vec::new(),
    };
    let root = match store {
        Ok(store) => store.root().to_path_buf(),
        Err(why) => return refused(why.clone()),
    };
    let Some(binary) = binary(home) else {
        let (install, refused) = match lacks(&["bash", "curl"]) {
            None => (
                Some(Install {
                    argv: vec!["sh".into(), "-c".into(), INSTALLER.into()],
                    about: format!(
                        "Cursor's own installer. It installs cursor-agent into ~/.local/bin: \
                         {INSTALLER}"
                    ),
                }),
                None,
            ),
            Some(why) => (None, Some(format!("Cursor's installer {why}"))),
        };
        return Planned {
            found: None,
            refused,
            entries: Vec::new(),
            notes: vec![
                "Not installed here. The driver found no executable. A configuration directory \
                 does not prove that an executable is installed."
                    .into(),
            ],
            install,
            needs: Vec::new(),
        };
    };
    let release = version(&binary).await.as_deref().and_then(release_within);
    let mut notes = Vec::new();
    if release.is_none() {
        notes.push(
            "`--version` did not answer, so the driver says nothing about this build.".into(),
        );
    }
    notes.extend(others_note(
        EXECUTABLE,
        EXECUTABLE,
        Some(&installer_folder(home)),
        &binary,
    ));
    let own = own(home);
    Planned {
        found: Some(Found {
            binary,
            release: release
                .map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"))
                .unwrap_or_default(),
            root,
        }),
        refused: None,
        entries: [
            Entry {
                path: own.join("mcp.json"),
                format: Format::Json,
                at: Some(format!("/mcpServers/{SERVER}")),
                value: json!({ "url": setup.mcp_url }),
                carries: vec![Part::Mcp],
            },
            Entry {
                path: own.join("skills/steery/SKILL.md"),
                format: Format::Text,
                at: None,
                value: json!(setup.skill),
                carries: vec![Part::Skill],
            },
        ]
        .into_iter()
        .chain(hook_entries(&own, &setup.hook))
        .collect(),
        notes,
        install: None,
        needs: Vec::new(),
    }
}

/// Steery's hooks in `~/.cursor/hooks.json`, beside its person's own, whatever moves the
/// chats. Cursor refuses a file with no number `version`, and knows only `1`.
fn hook_entries(own: &Path, hook: &str) -> Vec<Entry> {
    let path = own.join("hooks.json");
    let version = Entry {
        path: path.clone(),
        format: Format::Json,
        at: Some("/version".into()),
        value: json!(1),
        carries: vec![Part::Hooks],
    };
    let hooks = hooks::EVENTS.iter().map(|event| Entry {
        path: path.clone(),
        format: Format::Json,
        at: Some(format!("/hooks/{}/-", event.name)),
        value: json!({"command": format!("{hook} {}", event.name), "timeout": event.waits}),
        carries: vec![Part::Hooks],
    });
    std::iter::once(version).chain(hooks).collect()
}

/// The servers a chat loads: the person's in `~/.cursor/mcp.json`, and with `cwd` the
/// project's in `<cwd>/.cursor/mcp.json`, which Cursor ranks first. Cursor loads a project's
/// server only once its person approved it, and adds the servers of plugins and of a team:
/// the list is never complete.
fn mcp_list(home: &Path, asked: McpList) -> McpServers {
    let mut files = Vec::new();
    if let Some(cwd) = &asked.cwd {
        files.push(cwd.join(".cursor/mcp.json"));
    }
    files.push(own(home).join("mcp.json"));
    let mut servers = Servers::default();
    for config in files.iter().filter_map(|file| mcp::json(file)) {
        for (name, entry) in mcp::entries(&config) {
            servers.add(name, entry, true);
        }
    }
    McpServers {
        servers: servers.on(),
        complete: false,
        watch: files,
    }
}
