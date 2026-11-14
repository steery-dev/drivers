//! Where Warp keeps what it keeps, and its executable, as Warp reads them in
//! `crates/warp_core/src/paths.rs` and `app/src/persistence/sqlite.rs`. Each channel
//! (Stable, Preview) has its own state folder; in it, the app's `warp.sqlite` and the
//! terminal front end's `tui/warp.sqlite`, which Warp keeps apart so the two never migrate
//! one store under each other. On macOS the state folder is in Warp's app group container,
//! else in Application Support; on Linux it is the XDG state folder. Each channel also has
//! its own configuration folder in the home folder, `~/.warp` for Stable.

use std::path::{Path, PathBuf};

use steery_driver_common::program::{executable, on_path};

/// Warp's Apple team, whose app group holds the state folders on macOS.
const APP_GROUP: &str = "2BBY89MBSN.dev.warp";

/// A release channel: its macOS bundle id, its Linux folder name, and its configuration
/// folder's name.
struct Channel {
    bundle: &'static str,
    linux: &'static str,
    config: &'static str,
}

const CHANNELS: [Channel; 2] = [
    Channel {
        bundle: "dev.warp.Warp-Stable",
        linux: "warp-terminal",
        config: ".warp",
    },
    Channel {
        bundle: "dev.warp.Warp-Preview",
        linux: "warp-terminal-preview",
        config: ".warp-preview",
    },
];

/// The state folders that a channel's Warp may have kept, its app group's first.
fn state_dirs(home: &Path, channel: &Channel) -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        vec![
            home.join("Library/Group Containers")
                .join(APP_GROUP)
                .join("Library/Application Support")
                .join(channel.bundle),
            home.join("Library/Application Support")
                .join(channel.bundle),
        ]
    } else {
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .unwrap_or_else(|| home.join(".local/state"));
        vec![state.join(channel.linux)]
    }
}

/// Every `warp.sqlite` here, the app's and the terminal front end's of each channel.
pub(crate) fn stores(home: &Path) -> Vec<PathBuf> {
    CHANNELS
        .iter()
        .flat_map(|channel| state_dirs(home, channel))
        .flat_map(|dir| [dir.join("warp.sqlite"), dir.join("tui/warp.sqlite")])
        .filter(|db| db.is_file())
        .collect()
}

/// Warp's configuration folder, Stable's: `~/.warp`.
pub(crate) fn config(home: &Path) -> PathBuf {
    home.join(CHANNELS[0].config)
}

/// Every channel's configuration folder.
pub(crate) fn configs(home: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    CHANNELS
        .iter()
        .map(move |channel| home.join(channel.config))
}

/// The MCP file of a configuration folder. Warp starts every server in it on its own.
pub(crate) fn mcp_file(config: &Path) -> PathBuf {
    config.join(".mcp.json")
}

/// Warp's own skills folder, which Warp ranks first of the skills folders it reads.
pub(crate) fn skills(home: &Path) -> PathBuf {
    config(home).join("skills")
}

/// Warp's command, Stable's then Preview's: the app's own executable under another name,
/// which the app bundle keeps in `Contents/Resources/bin` (`script/macos/bundle`).
const COMMANDS: [(&str, &str); 2] = [("oz", "Warp.app"), ("oz-preview", "WarpPreview.app")];

/// Warp's executable: a command on `PATH`, else the one in an app bundle.
pub(crate) fn binary(home: &Path) -> Option<PathBuf> {
    COMMANDS
        .iter()
        .find_map(|(command, _)| on_path(command))
        .or_else(|| {
            COMMANDS.iter().find_map(|(command, app)| {
                [Path::new("/Applications"), &home.join("Applications")]
                    .iter()
                    .find_map(|dir| {
                        executable(dir.join(app).join("Contents/Resources/bin").join(command))
                    })
            })
        })
}

/// How to install Warp here, or why the driver cannot say, in one line.
pub(crate) fn install() -> Result<steery_sdk::wire::harness::Install, String> {
    if cfg!(target_os = "macos") && on_path("brew").is_some() {
        return Ok(steery_sdk::wire::harness::Install {
            argv: ["brew", "install", "--cask", "warp"]
                .map(str::to_owned)
                .to_vec(),
            about: "Warp from Homebrew: brew install --cask warp".into(),
        });
    }
    Err("Warp installs from warp.dev/download on this host".into())
}
