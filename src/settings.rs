//! Flat-file configuration (`romserver.toml`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

fn default_undo_dir() -> PathBuf {
    PathBuf::from("/config/undo")
}

#[derive(Debug, Deserialize)]
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
    /// "Emulators I use": chosen once, shared by every node.
    #[serde(default)]
    pub emulators: BTreeMap<String, Emulator>,
    #[serde(default)]
    pub nodes: BTreeMap<String, Node>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct System {
    /// Folder under `library_root`, e.g. `PSX`.
    pub folder: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Emulator {
    pub systems: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub frontend: Frontend,
    /// The node's checklist: which of the global emulators it actually has.
    pub emulators: Vec<String>,
    #[serde(default)]
    pub multi_disc: MultiDisc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Frontend {
    EsDe,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
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

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read settings {}", path.display()))?;
        let settings: Settings =
            toml::from_str(&text).with_context(|| format!("parse settings {}", path.display()))?;
        settings.validate()?;
        Ok(settings)
    }

    fn validate(&self) -> Result<()> {
        for (id, emu) in &self.emulators {
            for s in &emu.systems {
                if !self.systems.contains_key(s) {
                    bail!("emulator {id}: unknown system {s}");
                }
            }
        }
        for (name, node) in &self.nodes {
            if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
                bail!("node name {name:?} must be a plain folder name");
            }
            for e in &node.emulators {
                if !self.emulators.contains_key(e) {
                    bail!("node {name}: emulator {e} is not in the global emulator list");
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

    /// Systems a node can play: everything its ticked emulators cover.
    pub fn node_systems(&self, node: &Node) -> BTreeSet<&str> {
        node.emulators
            .iter()
            .flat_map(|e| &self.emulators[e].systems)
            .map(String::as_str)
            .collect()
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
