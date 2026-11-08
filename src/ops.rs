//! Operations shared by the CLI and the web UI.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::catalog::Catalog;
use crate::cuefix::{self, CueReport};
use crate::project::{self, Link};
use crate::scan::{self, Game};
use crate::settings::Settings;
use crate::sync::{self, Plan};
use crate::{media, platforms};

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

/// What a damaged game (a `.bin` with no cue sheet, on a system that takes cue sheets) is told.
pub fn damaged_message(bin: &Path) -> String {
    format!(
        "{}: damaged, missing its cue sheet. It won't show up on any machine. Give it a cue \
         sheet (or re-rip it); a cartridge dump needs the system's own extension.",
        bin.display()
    )
}

pub fn plan_node(
    settings: &Settings,
    catalog: &Catalog,
    node_name: &str,
    target: Target,
) -> Result<NodePlan> {
    let node = settings.node(node_name)?;
    let platforms = platforms::bundled();
    let layout = platforms.layout(platforms.platform(&node.platform)?);
    let owned = catalog.owned(node_name)?;
    let mut links: Vec<Link> = Vec::new();
    let mut warnings = Vec::new();
    if let Target::Settings { allow_empty } = target {
        for system in settings.node_systems(node) {
            let root = settings.system_root(system)?;
            let mut scan = scan::scan_system(&root)?;
            warnings.extend(scan.issues.iter().map(|i| format!("{system}: {i}")));
            // Damaged games (a .bin with no cue sheet where cue sheets are expected) stay out
            // of views, so no frontend lists an entry that can't play.
            if scan::is_cue_system(system) {
                let damaged = scan::lone_bins(&scan);
                scan.games.retain(|g| match g {
                    Game::Single { entry, .. } => !damaged.contains(&entry.file),
                    Game::MultiDisc { .. } => true,
                });
                warnings.extend(
                    damaged
                        .iter()
                        .map(|b| format!("{system}: {}", damaged_message(b))),
                );
            }
            let view_dir = layout.rom_dir(system);
            let projection = project::project_system(&view_dir, &root, &scan, node.multi_disc)?;
            warnings.extend(projection.warnings.iter().map(|w| format!("{system}: {w}")));
            if !layout.media.is_empty() {
                let source = settings.media_dir(system);
                let index = media::index_system(&source)?;
                links.extend(media::media_links(
                    &index,
                    &source,
                    &layout.media_base(system),
                    &layout.media,
                    &projection.entries,
                ));
            }
            let had_entries = owned.keys().any(|p| p.starts_with(&view_dir));
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

/// Brings a machine's view up to date by adding and relinking only: nothing is removed. Used
/// after new media arrives, without a preview, because it can't take anything away.
pub fn apply_additions(
    settings: &Settings,
    catalog: &mut Catalog,
    node_name: &str,
) -> Result<(usize, usize)> {
    let mut np = plan_node(
        settings,
        catalog,
        node_name,
        Target::Settings { allow_empty: false },
    )?;
    np.plan.remove.clear();
    let counts = (np.plan.create.len(), np.plan.replace.len());
    sync::apply(catalog, node_name, &np.view_root, &np.plan)?;
    Ok(counts)
}

/// The names a machine's frontend lists, which RetroArch also names saves after (the `.m3u`
/// name for a disc set shown as one game).
pub fn node_entry_names(settings: &Settings, node_name: &str) -> Result<Vec<String>> {
    let node = settings.node(node_name)?;
    let mut names = Vec::new();
    for system in settings.node_systems(node) {
        let root = settings.system_root(system)?;
        let scan = scan::scan_system(&root)?;
        let projection = project::project_system(Path::new(""), &root, &scan, node.multi_disc)?;
        names.extend(projection.entries.iter().filter_map(|e| {
            e.view_stem
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        }));
    }
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup();
    Ok(names)
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
    let mut scan = scan::scan_system(&root)?;
    // On systems that take cue sheets, a .bin is always a track; one without a cue sheet is a
    // disc rip missing its cue, or a legacy cartridge dump, neither of which is supported.
    if scan::is_cue_system(system) {
        for bin in scan::lone_bins(&scan) {
            scan.issues.push(damaged_message(&bin));
        }
    }
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
