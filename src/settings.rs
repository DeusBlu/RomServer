//! Flat-file configuration (`romserver.toml`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::platforms::RetroArchSort;

fn default_undo_dir() -> PathBuf {
    PathBuf::from("/config/undo")
}

fn default_unraid_share_config() -> Option<PathBuf> {
    Some(PathBuf::from("/unraid/shares"))
}

fn default_unraid_user_root() -> PathBuf {
    PathBuf::from("/mnt/user")
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Source share. Only the library tools ever write here.
    pub library_root: PathBuf,
    /// Must be reachable through the same mount as `library_root`, or hard links fail.
    pub views_root: PathBuf,
    /// Canonical save folders: one per system by default (`psx`), or as assigned
    /// (`snes-saves`). Defaults to `library_root/saves`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saves_root: Option<PathBuf>,
    /// Canonical scraped media, one folder per game: `<group>/<game>/<type>.<ext>`. Defaults to
    /// `library_root/media`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_root: Option<PathBuf>,
    /// Canonical save-state folders, one per emulator id. Defaults to `library_root/states`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub states_root: Option<PathBuf>,
    pub database: PathBuf,
    /// Where library tools keep originals of anything they change.
    #[serde(default = "default_undo_dir")]
    pub undo_dir: PathBuf,
    /// How clients reach the server.
    #[serde(default)]
    pub network: Network,
    /// Shares set by hand. They add to, or override by name, the ones read from Unraid.
    #[serde(default)]
    pub shares: BTreeMap<String, Share>,
    /// Keyed by short system name (`psx`), the canonical id used everywhere else.
    #[serde(default)]
    pub systems: BTreeMap<String, System>,
    /// "Emulators I use", per system: system → emulator ids (catalog or custom). Chosen once,
    /// shared by every machine.
    #[serde(default)]
    pub uses: Uses,
    /// Emulators that aren't in the bundled catalog.
    #[serde(default)]
    pub custom_emulators: BTreeMap<String, CustomEmulator>,
    /// system → emulator id → save folder name under `saves_root`, where it isn't the system's
    /// folder. Keyed by both because an emulator used for several systems (PicoDrive for
    /// genesis and segacd) keeps each system's saves apart. Emulators on one folder share
    /// saves where their formats match.
    #[serde(default)]
    pub save_folders: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    pub nodes: BTreeMap<String, Node>,
}

/// system → emulator ids. Ticking one emulator under several systems means "I use it for all
/// of these".
pub type Uses = BTreeMap<String, Vec<String>>;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct System {
    /// Folder under `library_root`, e.g. `PSX`.
    pub folder: PathBuf,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CustomEmulator {
    pub name: String,
    pub systems: Vec<String>,
}

/// Custom emulator ids carry this prefix so they can never collide with catalog ids.
pub const CUSTOM_PREFIX: &str = "custom-";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    /// Address clients use to reach the server (IP or host name). Empty until set.
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub protocol: Protocol,
    /// Unraid's `/boot/config/shares`, mounted read-only into the container. Missing is fine:
    /// then only the hand-set shares count.
    #[serde(default = "default_unraid_share_config")]
    pub unraid_share_config: Option<PathBuf>,
    /// Where Unraid user shares live. Container paths equal host paths, so this is the host's.
    #[serde(default = "default_unraid_user_root")]
    pub unraid_user_root: PathBuf,
}

impl Default for Network {
    fn default() -> Self {
        Network {
            server: String::new(),
            protocol: Protocol::default(),
            unraid_share_config: default_unraid_share_config(),
            unraid_user_root: default_unraid_user_root(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    #[default]
    Smb,
    Nfs,
}

impl Protocol {
    pub const ALL: &[Protocol] = &[Protocol::Smb, Protocol::Nfs];

    pub fn id(self) -> &'static str {
        match self {
            Protocol::Smb => "smb",
            Protocol::Nfs => "nfs",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Protocol::Smb => "SMB (cifs)",
            Protocol::Nfs => "NFS",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.id() == id)
    }
}

/// A folder the server exports, keyed by its share name.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Share {
    /// Absolute server path of the share's root.
    pub path: PathBuf,
    #[serde(default)]
    pub smb: bool,
    #[serde(default)]
    pub nfs: bool,
}

fn default_owner_id() -> u32 {
    1000
}

fn is_default_owner_id(id: &u32) -> bool {
    *id == default_owner_id()
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    /// Platform id from `assets/platforms.toml` (`es-de`, `emudeck`, `retrodeck`, …).
    #[serde(alias = "frontend")]
    pub platform: String,
    /// The machine's checklist: the subset of the global `uses` it actually has.
    #[serde(default)]
    pub uses: Uses,
    #[serde(default)]
    pub multi_disc: MultiDisc,
    /// Home folder on the machine. Defaults to the platform's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    /// The platform's base folder, when it isn't the default (EmuDeck on an SD card).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Owner of mounted files on the machine.
    #[serde(
        default = "default_owner_id",
        skip_serializing_if = "is_default_owner_id"
    )]
    pub uid: u32,
    #[serde(
        default = "default_owner_id",
        skip_serializing_if = "is_default_owner_id"
    )]
    pub gid: u32,
    /// Path of an SMB credentials file on the machine. RomServer never sees its contents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<String>,
    /// How RetroArch sorts saves on this machine, when it differs from the platform default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retroarch_sort: Option<RetroArchSort>,
    /// Also mount save-state folders (states are emulator-version specific).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sync_states: bool,
}

impl Node {
    pub fn new(platform: &str, multi_disc: MultiDisc) -> Self {
        Node {
            platform: platform.to_string(),
            uses: Uses::new(),
            multi_disc,
            home: None,
            base: None,
            uid: default_owner_id(),
            gid: default_owner_id(),
            credentials: None,
            retroarch_sort: None,
            sync_states: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MultiDisc {
    /// `.m3u` plus extensionless cue sheets: one entry per game.
    #[default]
    M3u,
    /// No `.m3u`; every disc shown, with `.cue` added back.
    Discs,
    /// ES-DE "directories interpreted as files": a `Game.m3u/` folder per set.
    M3uDirectory,
}

impl MultiDisc {
    pub const ALL: &[MultiDisc] = &[MultiDisc::M3u, MultiDisc::Discs, MultiDisc::M3uDirectory];

    pub fn id(self) -> &'static str {
        match self {
            MultiDisc::M3u => "m3u",
            MultiDisc::Discs => "discs",
            MultiDisc::M3uDirectory => "m3u-directory",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MultiDisc::M3u => "One entry per game (.m3u)",
            MultiDisc::Discs => "Every disc listed (.cue, no .m3u)",
            MultiDisc::M3uDirectory => "ES-DE folder per game (Game.m3u/)",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|m| m.id() == id)
    }
}

/// Ids double as folder names (views) and URL segments, so keep them boring.
pub fn is_plain_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read settings {}", path.display()))?;
        let settings: Settings =
            toml::from_str(&text).with_context(|| format!("parse settings {}", path.display()))?;
        settings.validate()?;
        Ok(settings)
    }

    /// Validates, then replaces the file atomically. Comments in the file are not preserved.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        crate::ownership::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        for (path, what) in [
            (&self.library_root, "library_root"),
            (&self.views_root, "views_root"),
        ] {
            if !path.is_absolute() {
                bail!("{what} must be an absolute path");
            }
        }
        for (path, what) in [
            (&self.saves_root, "saves_root"),
            (&self.media_root, "media_root"),
            (&self.states_root, "states_root"),
        ] {
            if path.as_ref().is_some_and(|p| !p.is_absolute()) {
                bail!("{what} must be an absolute path");
            }
        }
        for (id, sys) in &self.systems {
            if !is_plain_id(id) {
                bail!("system id {id:?} may only use letters, digits, - and _");
            }
            if !is_plain_relative(&sys.folder) {
                bail!(
                    "system {id}: folder {} must be a plain folder under library_root",
                    sys.folder.display()
                );
            }
        }
        for (name, share) in &self.shares {
            if name.is_empty() || name.contains('/') {
                bail!("share name {name:?} can't be empty or contain /");
            }
            if !share.path.is_absolute() {
                bail!("share {name}: path must be absolute");
            }
        }
        for (id, emu) in &self.custom_emulators {
            if !is_plain_id(id) || !id.starts_with(CUSTOM_PREFIX) {
                bail!(
                    "custom emulator id {id:?} must be {CUSTOM_PREFIX}<letters, digits, - and _>"
                );
            }
            for s in &emu.systems {
                if !self.systems.contains_key(s) {
                    bail!("custom emulator {id}: unknown system {s}");
                }
            }
        }
        for (system, ids) in &self.uses {
            if !self.systems.contains_key(system) {
                bail!("uses: unknown system {system}");
            }
            for id in ids {
                if !is_plain_id(id) {
                    bail!("uses.{system}: bad emulator id {id:?}");
                }
                let custom_ok = || {
                    self.custom_emulators
                        .get(id)
                        .is_some_and(|c| c.systems.contains(system))
                };
                if id.starts_with(CUSTOM_PREFIX) && !custom_ok() {
                    bail!(
                        "uses.{system}: custom emulator {id} doesn't exist or doesn't run {system}"
                    );
                }
            }
        }
        for (system, folders) in &self.save_folders {
            for (id, folder) in folders {
                if !is_folder_name(folder) {
                    bail!(
                        "save folder for {id} on {system}: {folder:?} must be a single folder name"
                    );
                }
            }
        }
        let platforms = crate::platforms::bundled();
        for (name, node) in &self.nodes {
            if !is_plain_id(name) {
                bail!("machine name {name:?} may only use letters, digits, - and _");
            }
            platforms
                .platform(&node.platform)
                .with_context(|| format!("machine {name}"))?;
            for (value, what) in [(&node.home, "home"), (&node.credentials, "credentials")] {
                if value.as_ref().is_some_and(|v| !v.starts_with('/')) {
                    bail!("machine {name}: {what} must be an absolute path");
                }
            }
            for (system, ids) in &node.uses {
                for id in ids {
                    if !self.uses.get(system).is_some_and(|g| g.contains(id)) {
                        bail!(
                            "machine {name}: {id} for {system} isn't in the global emulator choices"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    pub fn node(&self, name: &str) -> Result<&Node> {
        self.nodes
            .get(name)
            .with_context(|| format!("no node named {name}"))
    }

    /// Systems a machine can play: those with at least one ticked emulator.
    pub fn node_systems<'a>(&self, node: &'a Node) -> BTreeSet<&'a str> {
        node.uses
            .iter()
            .filter(|(_, ids)| !ids.is_empty())
            .map(|(s, _)| s.as_str())
            .collect()
    }

    /// Drops machine ticks that are no longer in the global choices, and empty lists.
    pub fn prune_uses(&mut self) {
        self.uses.retain(|_, ids| !ids.is_empty());
        for node in self.nodes.values_mut() {
            for (system, ids) in node.uses.iter_mut() {
                let global = self.uses.get(system);
                ids.retain(|id| global.is_some_and(|g| g.contains(id)));
            }
            node.uses.retain(|_, ids| !ids.is_empty());
        }
    }

    pub fn system_root(&self, system: &str) -> Result<PathBuf> {
        let sys = self
            .systems
            .get(system)
            .with_context(|| format!("unknown system {system}"))?;
        Ok(self.library_root.join(&sys.folder))
    }

    pub fn view_root(&self, node: &str) -> PathBuf {
        self.views_root.join(node)
    }

    pub fn saves_root(&self) -> PathBuf {
        self.saves_root
            .clone()
            .unwrap_or_else(|| self.library_root.join("saves"))
    }

    pub fn media_root(&self) -> PathBuf {
        self.media_root
            .clone()
            .unwrap_or_else(|| self.library_root.join("media"))
    }

    /// A system's media folder, shared with the platform's other names (tg16 uses pcengine's).
    pub fn media_dir(&self, system: &str) -> PathBuf {
        self.media_root()
            .join(crate::platforms::media_group(system))
    }

    pub fn states_root(&self) -> PathBuf {
        self.states_root
            .clone()
            .unwrap_or_else(|| self.library_root.join("states"))
    }

    /// The save folder an emulator writes to for one system: its assignment, or the system's.
    pub fn save_folder(&self, system: &str, emulator: &str) -> String {
        match self.save_folders.get(system).and_then(|f| f.get(emulator)) {
            Some(folder) => folder.clone(),
            None => Self::default_save_folder(system),
        }
    }

    /// Where a system's saves go unless assigned elsewhere: its own folder (by media group, so
    /// tg16 and pcengine share), so every emulator for a system shares saves where their
    /// formats match and keeps its own file beside the others where they don't.
    pub fn default_save_folder(system: &str) -> String {
        crate::platforms::media_group(system)
    }

    /// Removes a system and every choice that depended on it. Links already in views stay until
    /// each machine's view is applied again.
    pub fn remove_system(&mut self, system: &str) {
        self.systems.remove(system);
        self.uses.remove(system);
        self.save_folders.remove(system);
        for node in self.nodes.values_mut() {
            node.uses.remove(system);
        }
        for emu in self.custom_emulators.values_mut() {
            emu.systems.retain(|s| s != system);
        }
        let orphans: Vec<String> = self
            .custom_emulators
            .iter()
            .filter(|(_, e)| e.systems.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        for id in &orphans {
            self.custom_emulators.remove(id);
            for ids in self.uses.values_mut() {
                ids.retain(|e| e != id);
            }
        }
        self.prune_uses();
    }
}

/// One path component: no separators, not `.` or `..`, not hidden.
pub fn is_folder_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0'])
}

/// A relative path that stays inside its parent: no root, no `..`, not empty.
pub fn is_plain_relative(path: &Path) -> bool {
    use std::path::Component;
    path.components().next().is_some()
        && path.components().all(|c| matches!(c, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Settings {
        let s: Settings = toml::from_str(include_str!("../romserver.example.toml")).unwrap();
        s.validate().unwrap();
        s
    }

    #[test]
    fn example_settings_are_valid() {
        let s = example();
        assert_eq!(
            s.node_systems(s.node("desktop").unwrap()),
            BTreeSet::from(["ps2", "psx", "snes"])
        );
    }

    #[test]
    fn machines_can_only_tick_global_choices_and_prune_follows() {
        let mut s = example();
        let desktop = s.nodes.get_mut("desktop").unwrap();
        desktop
            .uses
            .get_mut("snes")
            .unwrap()
            .push("ra-bsnes".into());
        assert!(s.validate().is_err(), "bsnes isn't a global choice");
        s.uses
            .get_mut("psx")
            .unwrap()
            .retain(|e| e != "ra-mednafen_psx");
        s.prune_uses();
        let desktop = s.node("desktop").unwrap();
        assert!(
            !desktop.uses.contains_key("psx"),
            "unticked globally, so dropped from machines"
        );
        assert!(!desktop.uses["snes"].contains(&"ra-bsnes".to_string()));
        s.validate().unwrap();
    }

    #[test]
    fn removing_a_system_drops_everything_that_depended_on_it() {
        let mut s = example();
        s.custom_emulators.insert(
            "custom-snes-only".into(),
            CustomEmulator {
                name: "SNES only".into(),
                systems: vec!["snes".into()],
            },
        );
        s.uses
            .get_mut("snes")
            .unwrap()
            .push("custom-snes-only".into());
        s.validate().unwrap();
        s.remove_system("snes");
        s.validate().unwrap();
        assert!(!s.systems.contains_key("snes"));
        assert!(!s.uses.contains_key("snes"));
        assert!(!s.custom_emulators.contains_key("custom-snes-only"));
        assert!(!s.node("desktop").unwrap().uses.contains_key("snes"));
        assert!(!s.node("pi2").unwrap().uses.contains_key("snes"));
    }

    #[test]
    fn emulators_save_into_their_systems_folder_unless_assigned() {
        let mut s = example();
        // Both PSX emulators share the system's folder, so a save in a shared format is one save.
        assert_eq!(s.save_folder("psx", "ra-mednafen_psx"), "psx");
        assert_eq!(s.save_folder("psx", "duckstation"), "psx");
        // Assigned folders win.
        assert_eq!(s.save_folder("snes", "ra-snes9x"), "snes-saves");
        // One emulator for two systems keeps each system's saves apart.
        assert_eq!(s.save_folder("genesis", "ra-genesis_plus_gx"), "genesis");
        assert_eq!(s.save_folder("segacd", "ra-genesis_plus_gx"), "segacd");
        // Same platform under another name: the shared group folder.
        assert_eq!(s.save_folder("tg16", "ra-pcee2"), "pcengine");
        // Removing a system drops its assignments.
        s.remove_system("snes");
        assert!(!s.save_folders.contains_key("snes"));
    }

    #[test]
    fn system_folders_must_stay_under_the_library() {
        let mut s = example();
        s.systems.get_mut("psx").unwrap().folder = PathBuf::from("../etc");
        assert!(s.validate().is_err());
        s.systems.get_mut("psx").unwrap().folder = PathBuf::from("/etc");
        assert!(s.validate().is_err());
    }
}
