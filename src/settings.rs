//! Flat-file configuration (`romserver.toml`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

fn default_undo_dir() -> PathBuf {
    PathBuf::from("/config/undo")
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Source share. Only the library tools ever write here.
    pub library_root: PathBuf,
    /// Must be reachable through the same mount as `library_root`, or hard links fail.
    pub views_root: PathBuf,
    pub database: PathBuf,
    /// Where library tools keep originals of anything they change.
    #[serde(default = "default_undo_dir")]
    pub undo_dir: PathBuf,
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
pub struct Node {
    pub frontend: Frontend,
    /// The machine's checklist: the subset of the global `uses` it actually has.
    #[serde(default)]
    pub uses: Uses,
    #[serde(default)]
    pub multi_disc: MultiDisc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Frontend {
    EsDe,
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

impl Frontend {
    pub const ALL: &[Frontend] = &[Frontend::EsDe];

    pub fn id(self) -> &'static str {
        match self {
            Frontend::EsDe => "es-de",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Frontend::EsDe => "ES-DE",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|f| f.id() == id)
    }
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
        std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
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
        for (name, node) in &self.nodes {
            if !is_plain_id(name) {
                bail!("machine name {name:?} may only use letters, digits, - and _");
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
}
