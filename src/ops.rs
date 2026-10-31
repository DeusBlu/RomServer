//! Operations shared by the CLI and the web UI.

use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::catalog::Catalog;
use crate::cuefix::{self, CueReport};
use crate::project::{self, Link};
use crate::scan::{self, Game};
use crate::settings::Settings;
use crate::sync::{self, Plan};

pub struct NodePlan {
    pub plan: Plan,
    pub view_root: PathBuf,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// What the node's settings say the view should contain.
    Settings { allow_empty: bool },
    /// Remove everything RomServer put in the view.
    Empty,
}

pub fn plan_node(
    settings: &Settings,
    catalog: &Catalog,
    node_name: &str,
    target: Target,
) -> Result<NodePlan> {
    let node = settings.node(node_name)?;
    let owned = catalog.owned(node_name)?;
    let mut links: Vec<Link> = Vec::new();
    let mut warnings = Vec::new();
    if let Target::Settings { allow_empty } = target {
        for system in settings.node_systems(node) {
            let root = settings.system_root(system)?;
            let scan = scan::scan_system(&root)?;
            warnings.extend(scan.issues.iter().map(|i| format!("{system}: {i}")));
            let projection =
                project::project_system(node.frontend, system, &root, &scan, node.multi_disc)?;
            warnings.extend(projection.warnings.iter().map(|w| format!("{system}: {w}")));
            let had_entries = owned.keys().any(|p| p.starts_with(system));
            if projection.links.is_empty() && had_entries && !allow_empty {
                bail!(
                    "{system}: library folder {} has no games but the view had some. \
                     Is the share mounted? If this is intended, allow empty systems.",
                    root.display()
                );
            }
            links.extend(projection.links);
        }
    }
    let view_root = settings.view_root(node_name);
    Ok(NodePlan {
        plan: sync::plan(&view_root, links, &owned)?,
        view_root,
        warnings,
    })
}

pub struct SystemSummary {
    pub system: String,
    pub root: PathBuf,
    pub games: usize,
    pub multi_disc: usize,
    pub saves: usize,
    pub ignored: usize,
    pub issues: Vec<String>,
}

pub fn summarise_system(settings: &Settings, system: &str) -> Result<SystemSummary> {
    let root = settings.system_root(system)?;
    let scan = scan::scan_system(&root)?;
    Ok(SystemSummary {
        system: system.to_string(),
        multi_disc: scan
            .games
            .iter()
            .filter(|g| matches!(g, Game::MultiDisc { .. }))
            .count(),
        games: scan.games.len(),
        saves: scan.saves.len(),
        ignored: scan.ignored.len(),
        issues: scan.issues,
        root,
    })
}

pub fn check_cues(settings: &Settings, only: Option<&str>) -> Result<Vec<CueReport>> {
    let mut reports = Vec::new();
    for system in settings
        .systems
        .keys()
        .filter(|s| only.is_none_or(|o| o == s.as_str()))
    {
        reports.extend(cuefix::check(&settings.system_root(system)?)?);
    }
    Ok(reports)
}
