//! Saves and save states. The library keeps them by system and emulator, never in a frontend's
//! shape: `saves/<save folder>/` for each (system, emulator) pair, `states/<emulator>/` for each
//! emulator. Each machine's view projects them, as hard links, into the folders that machine's
//! emulators actually use (RetroArch sorted by core puts Genesis and Sega CD saves for one core
//! side by side; sorted by content folder it keeps them apart). The view mirrors the machine's
//! own paths under `machine/`, so one mount covers a folder and everything inside it.
//!
//! An emulator that rewrites a save in place writes straight through the link into the library.
//! A save a machine creates arrives as a real file in its view; `plan_ingest` and `apply_ingest`
//! bring it into the library (the monitor runs them), after which every other view links it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::catalog::Catalog;
use crate::emulators::{EmulatorInfo, Kind};
use crate::platforms::{self, RetroArchSort};
use crate::project::{self, Link};
use crate::scan::{self, file_name};
use crate::settings::Settings;
use crate::sync::IMPORT_PREFIX;

/// The view folder that mirrors a machine's own paths.
pub const VIEW_MACHINE: &str = "machine";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveKind {
    Saves,
    States,
}

impl SaveKind {
    fn label(self) -> &'static str {
        match self {
            SaveKind::Saves => "Saves",
            SaveKind::States => "States",
        }
    }
}

/// One library folder feeding a machine's save folder, and whose saves it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveSource {
    pub system: String,
    pub emulator: String,
    pub library: PathBuf,
}

/// A folder on a machine where emulators keep saves or states, and the library folders that
/// fill it.
#[derive(Debug, Clone)]
pub struct SaveDir {
    pub kind: SaveKind,
    pub label: String,
    /// Path on the machine.
    pub local: PathBuf,
    pub sources: Vec<SaveSource>,
}

impl SaveDir {
    /// Where this folder lives in the machine's view, relative to the view root.
    pub fn view_path(&self) -> PathBuf {
        machine_path(&self.local)
    }

    /// The library folders feeding this one, without repeats, in order.
    pub fn libraries(&self) -> Vec<&Path> {
        let mut out: Vec<&Path> = Vec::new();
        for s in &self.sources {
            if !out.contains(&s.library.as_path()) {
                out.push(&s.library);
            }
        }
        out
    }
}

/// A machine path's place in the view.
pub fn machine_path(local: &Path) -> PathBuf {
    Path::new(VIEW_MACHINE).join(local.strip_prefix("/").unwrap_or(local))
}

/// Every save and state folder a machine's emulators use, with the library folders that fill
/// each. Emulators RomServer can't place (no core name, unknown save path) become warnings.
pub fn save_dirs(
    settings: &Settings,
    node_name: &str,
    emulators: &HashMap<&str, &EmulatorInfo>,
) -> Result<(Vec<SaveDir>, Vec<String>)> {
    let node = settings.node(node_name)?;
    let platforms = platforms::bundled();
    let platform = platforms.platform(&node.platform)?;
    let layout = platforms.layout(platform);
    let home = node.home.as_deref().unwrap_or(&platform.default_home);
    let base = node.base.as_deref().unwrap_or(&platform.base);
    let local = |template: &str| platforms::expand(template, home, base);
    let sort = node.retroarch_sort.unwrap_or(platform.retroarch_sort);
    let name = |id: &str| emulators.get(id).map_or(id.to_string(), |e| e.name.clone());
    let mut warnings = Vec::new();
    // Keyed by the machine path; emulators sharing a folder share its entry.
    let mut dirs: BTreeMap<PathBuf, (SaveDir, BTreeSet<String>)> = BTreeMap::new();
    let mut add = |kind: SaveKind, local: PathBuf, label: String, who: String, src: SaveSource| {
        let entry = dirs.entry(local.clone()).or_insert_with(|| {
            (
                SaveDir {
                    kind,
                    label,
                    local,
                    sources: Vec::new(),
                },
                BTreeSet::new(),
            )
        });
        entry.1.insert(who);
        if !entry.0.sources.contains(&src) {
            entry.0.sources.push(src);
        }
    };
    let library = |kind: SaveKind, system: &str, id: &str| match kind {
        SaveKind::Saves => settings.saves_root().join(settings.save_folder(system, id)),
        SaveKind::States => settings.states_root().join(id),
    };
    let kinds: &[SaveKind] = if node.sync_states {
        &[SaveKind::Saves, SaveKind::States]
    } else {
        &[SaveKind::Saves]
    };

    // RetroArch cores, in the folders RetroArch arranges them in on this machine.
    for (system, ids) in &node.uses {
        for id in ids {
            let Some(info) = emulators.get(id.as_str()) else {
                continue;
            };
            if info.kind != Kind::RetroArch {
                continue;
            }
            for &kind in kinds {
                let ra_root = local(match kind {
                    SaveKind::Saves => &platform.retroarch_saves,
                    SaveKind::States => &platform.retroarch_states,
                });
                let (dir, label) = match sort {
                    RetroArchSort::Core => match &info.corename {
                        Some(core) => (ra_root.join(core), core.clone()),
                        None => {
                            if kind == SaveKind::Saves {
                                warnings.push(format!(
                                    "{}: no RetroArch core name known; its saves aren't shared",
                                    info.name
                                ));
                            }
                            continue;
                        }
                    },
                    RetroArchSort::Content => {
                        let folder = layout.folder(system).to_string();
                        (ra_root.join(&folder), format!("RetroArch, {folder}"))
                    }
                    RetroArchSort::None => (ra_root, "RetroArch".to_string()),
                };
                let src = SaveSource {
                    system: system.clone(),
                    emulator: id.clone(),
                    library: library(kind, system, id),
                };
                add(kind, dir, label, info.name.clone(), src);
            }
        }
    }

    // Standalone and custom emulators: the platform's known paths per slot. A slot holding one
    // system's saves (Dolphin's GC/USA is gc's, Wii is wii's) is fed by that system's folder,
    // and only where the machine uses the emulator for it; a slot holding every system's saves
    // is fed by each system's folder.
    let standalone: BTreeSet<&str> = node
        .uses
        .values()
        .flatten()
        .map(String::as_str)
        .filter(|id| !emulators.get(id).is_some_and(|e| e.kind == Kind::RetroArch))
        .collect();
    for id in standalone {
        let systems: Vec<&str> = node
            .uses
            .iter()
            .filter(|(_, ids)| ids.iter().any(|e| e == id))
            .map(|(system, _)| system.as_str())
            .collect();
        for &kind in kinds {
            let table = match kind {
                SaveKind::Saves => &platform.saves,
                SaveKind::States => &platform.states,
            };
            let Some(slots) = table.get(id) else {
                if kind == SaveKind::Saves {
                    warnings.push(format!(
                        "{}: RomServer doesn't know where it keeps saves on {}; its saves aren't \
                         shared",
                        name(id),
                        platform.label
                    ));
                }
                continue;
            };
            for (slot, template) in slots {
                let label = match slot.as_str() {
                    "" => name(id),
                    slot => format!("{} {slot}", name(id)),
                };
                let for_systems: Vec<&str> = match platforms.slot_system(id, slot) {
                    Some(system) if systems.contains(&system) => vec![system],
                    Some(_) => continue,
                    None => systems.clone(),
                };
                for system in for_systems {
                    let mut lib = library(kind, system, id);
                    if !slot.is_empty() {
                        lib.push(slot);
                    }
                    let src = SaveSource {
                        system: system.to_string(),
                        emulator: id.to_string(),
                        library: lib,
                    };
                    add(kind, local(template), label.clone(), name(id), src);
                }
            }
        }
    }

    let out = dirs
        .into_values()
        .map(|(mut dir, names)| {
            let names = names.into_iter().collect::<Vec<_>>().join(", ");
            dir.label = if dir.label == names {
                format!("{}: {}", dir.kind.label(), dir.label)
            } else {
                format!("{}: {} ({names})", dir.kind.label(), dir.label)
            };
            dir
        })
        .collect();
    Ok((out, warnings))
}

/// Files under `dir`, relative to it, sorted. A missing folder has none.
fn walk(dir: &Path) -> Result<Vec<PathBuf>> {
    fn inner(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        let here = root.join(rel);
        let entries = match std::fs::read_dir(&here) {
            Ok(e) => e,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("read {}", here.display())),
        };
        for entry in entries {
            let entry = entry?;
            let path = rel.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                inner(root, &path, out)?;
            } else {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    inner(dir, Path::new(""), &mut out)?;
    out.sort();
    Ok(out)
}

/// The links that project a machine's saves and states into its view. Two library files that
/// would land on one name (case-insensitively, as SMB sees it) keep the first and warn.
pub fn save_links(dirs: &[SaveDir], warnings: &mut Vec<String>) -> Result<Vec<Link>> {
    let mut taken: HashMap<String, PathBuf> = HashMap::new();
    let mut links = Vec::new();
    for dir in dirs {
        let view = dir.view_path();
        for library in dir.libraries() {
            for rel in walk(library)? {
                if file_name(&rel).starts_with(IMPORT_PREFIX) {
                    continue;
                }
                let dst = view.join(&rel);
                let src = library.join(&rel);
                let key = dst.to_string_lossy().to_lowercase();
                if let Some(first) = taken.get(&key) {
                    warnings.push(format!(
                        "{}: both {} and {} would go here; only the first is shown",
                        dst.display(),
                        first.display(),
                        src.display()
                    ));
                    continue;
                }
                taken.insert(key, src.clone());
                links.push(Link { dst, src });
            }
        }
    }
    Ok(links)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestAction {
    /// The library has no save by this name yet.
    New,
    /// The machine replaced RomServer's link with a new file (an emulator that saves by
    /// writing a new file): the newer save wins; the library's old one goes to the undo folder.
    Update,
    /// The library already has this exact file: the view's copy just becomes a link to it.
    Relink,
}

#[derive(Debug, Clone)]
pub struct IngestItem {
    /// Relative to the machine's view root.
    pub view_path: PathBuf,
    /// Absolute, in the library.
    pub target: PathBuf,
    pub action: IngestAction,
}

#[derive(Debug, Default)]
pub struct IngestPlan {
    pub items: Vec<IngestItem>,
    /// Real files that stay where they are, and why.
    pub problems: Vec<String>,
}

/// The names a machine lists for a system (what RetroArch names saves after), lowercased.
fn entry_names(settings: &Settings, node_name: &str, system: &str) -> Result<HashSet<String>> {
    let node = settings.node(node_name)?;
    let root = settings.system_root(system)?;
    let scan = scan::scan_system(&root)?;
    let projection = project::project_system(Path::new(""), &root, &scan, node.multi_disc)?;
    Ok(projection
        .entries
        .iter()
        .filter_map(|e| e.view_stem.file_name())
        .map(|n| n.to_string_lossy().to_lowercase())
        .collect())
}

/// The game names a save file could be named after: `Game.srm` → `game`, `Game.state1.png` →
/// `game` (names may contain dots themselves, so every shorter form is a candidate).
fn name_candidates(file: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = file.to_lowercase();
    for _ in 0..3 {
        match rest.rsplit_once('.') {
            Some((stem, _)) if !stem.is_empty() => {
                out.push(stem.to_string());
                rest = stem.to_string();
            }
            _ => break,
        }
    }
    out
}

/// Whether two files hold the same bytes.
fn same_content(a: &Path, b: &Path) -> Result<bool> {
    if std::fs::metadata(a)?.len() != std::fs::metadata(b)?.len() {
        return Ok(false);
    }
    Ok(std::fs::read(a)? == std::fs::read(b)?)
}

/// Finds the files a machine wrote into its save and state folders (anything that isn't still
/// RomServer's link) and works out where each belongs in the library. Read-only.
pub fn plan_ingest(
    settings: &Settings,
    catalog: &Catalog,
    node_name: &str,
    emulators: &HashMap<&str, &EmulatorInfo>,
) -> Result<IngestPlan> {
    let (dirs, _) = save_dirs(settings, node_name, emulators)?;
    let view_root = settings.view_root(node_name);
    let owned = catalog.owned(node_name)?;
    let mut names: HashMap<String, HashSet<String>> = HashMap::new();
    let mut plan = IngestPlan::default();
    let views: Vec<PathBuf> = dirs.iter().map(SaveDir::view_path).collect();
    for (dir, view) in dirs.iter().zip(&views) {
        for rel in walk(&view_root.join(view))? {
            let view_path = view.join(&rel);
            // A file inside a nested save folder belongs to that one.
            let deepest = views
                .iter()
                .filter(|v| view_path.starts_with(v))
                .max_by_key(|v| v.components().count());
            if deepest != Some(view) || file_name(&rel).starts_with(IMPORT_PREFIX) {
                continue;
            }
            let meta = std::fs::symlink_metadata(view_root.join(&view_path))?;
            if !meta.is_file() {
                continue;
            }
            let replaced = match owned.get(&view_path) {
                Some(o) if crate::sync::still_ours(o, &meta) => continue, // a projection
                Some(_) => true,
                None => false,
            };

            let libraries = dir.libraries();
            let library = if let [only] = libraries.as_slice() {
                *only
            } else {
                // Several library folders feed this one (one core for two systems): the
                // game's name says whose save it is.
                let candidates = name_candidates(file_name(&rel));
                let mut matches = Vec::new();
                for library in &libraries {
                    let systems: Vec<&str> = dir
                        .sources
                        .iter()
                        .filter(|s| s.library == *library)
                        .map(|s| s.system.as_str())
                        .collect();
                    let mut hit = false;
                    for system in &systems {
                        if !names.contains_key(*system) {
                            names.insert(
                                system.to_string(),
                                entry_names(settings, node_name, system)?,
                            );
                        }
                        hit |= candidates.iter().any(|c| names[*system].contains(c));
                    }
                    if hit {
                        matches.push((*library, systems));
                    }
                }
                match matches.as_slice() {
                    [(library, _)] => *library,
                    [] => {
                        plan.problems.push(format!(
                            "{}: not named after a game this machine lists for {}; left in the \
                             view",
                            view_path.display(),
                            dir.sources
                                .iter()
                                .map(|s| s.system.as_str())
                                .collect::<BTreeSet<_>>()
                                .into_iter()
                                .collect::<Vec<_>>()
                                .join(" or ")
                        ));
                        continue;
                    }
                    _ => {
                        let systems: BTreeSet<&str> = matches
                            .iter()
                            .flat_map(|(_, s)| s.iter().copied())
                            .collect();
                        plan.problems.push(format!(
                            "{}: a game by this name exists for {}; can't tell whose save it is, \
                             left in the view",
                            view_path.display(),
                            systems.into_iter().collect::<Vec<_>>().join(" and ")
                        ));
                        continue;
                    }
                }
            };
            let target = library.join(&rel);
            let action = if !target.exists() {
                IngestAction::New
            } else if replaced {
                IngestAction::Update
            } else if same_content(&view_root.join(&view_path), &target)? {
                IngestAction::Relink
            } else {
                plan.problems.push(format!(
                    "{}: the library already has a different {}; kept both, nothing replaced",
                    view_path.display(),
                    target.display()
                ));
                continue;
            };
            plan.items.push(IngestItem {
                view_path,
                target,
                action,
            });
        }
    }
    Ok(plan)
}

/// Carries out an ingest plan for one machine: each file goes into the library (an updated save
/// replaces the library's, which is kept in the undo folder) and the view's file becomes a link
/// to it. Returns how many files were brought in.
pub fn apply_ingest(
    settings: &Settings,
    catalog: &mut Catalog,
    node_name: &str,
    plan: &IngestPlan,
) -> Result<usize> {
    if plan.items.is_empty() {
        return Ok(0);
    }
    let view_root = settings.view_root(node_name);
    let undo_dir = &settings.undo_dir;
    crate::ownership::create_dir_all(undo_dir)
        .with_context(|| format!("create {}", undo_dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let log = undo_dir.join(format!("saves-ingest-{node_name}-{stamp}.tsv"));
    let mut out =
        std::fs::File::create(&log).with_context(|| format!("create {}", log.display()))?;
    crate::ownership::own(&log)?;
    writeln!(out, "# action\tview file\tlibrary file\tkept at")?;
    let mut recorded = Vec::new();
    for item in &plan.items {
        let view_file = view_root.join(&item.view_path);
        let id = match item.action {
            IngestAction::New => crate::sync::import(&view_file, &item.target, stamp)?,
            IngestAction::Update => {
                let rel = item
                    .target
                    .strip_prefix(&settings.library_root)
                    .unwrap_or(&item.target);
                let kept = undo_dir.join(format!("saves-replaced-{stamp}")).join(rel);
                crate::ownership::move_file(&item.target, &kept)?;
                writeln!(
                    out,
                    "replaced\t\t{}\t{}",
                    item.target.display(),
                    kept.display()
                )?;
                crate::sync::import(&view_file, &item.target, stamp)?
            }
            IngestAction::Relink => {
                std::fs::remove_file(&view_file)
                    .with_context(|| format!("remove {}", view_file.display()))?;
                crate::sync::make_link(&item.target, &view_file)?
            }
        };
        writeln!(
            out,
            "{:?}\t{}\t{}",
            item.action,
            view_file.display(),
            item.target.display()
        )?;
        recorded.push((item.view_path.clone(), item.target.clone(), id));
    }
    out.sync_all()?;
    let tx = catalog.transaction()?;
    for (path, source, id) in &recorded {
        crate::catalog::record(&tx, node_name, path, source, *id)?;
    }
    tx.commit()?;
    Ok(plan.items.len())
}

/// Brings one machine's save and state projections up to date, and makes sure every folder its
/// mounts point at exists. Only the `machine/` part of the view is touched.
pub fn sync_view(
    settings: &Settings,
    catalog: &mut Catalog,
    node_name: &str,
    emulators: &HashMap<&str, &EmulatorInfo>,
) -> Result<Vec<String>> {
    let (dirs, mut warnings) = save_dirs(settings, node_name, emulators)?;
    let view_root = settings.view_root(node_name);
    for dir in &dirs {
        let path = view_root.join(dir.view_path());
        crate::ownership::create_dir_all(&path)
            .with_context(|| format!("create {}", path.display()))?;
    }
    let links = save_links(&dirs, &mut warnings)?;
    let owned: BTreeMap<_, _> = catalog
        .owned(node_name)?
        .into_iter()
        .filter(|(p, _)| p.starts_with(VIEW_MACHINE))
        .collect();
    let plan = crate::sync::plan(&view_root, links, &owned)?;
    warnings.extend(plan.conflicts.iter().cloned());
    crate::sync::apply(catalog, node_name, &view_root, &plan)?;
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;
    use crate::settings::Node;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    /// A library with Genesis and Sega CD games, both played with one RetroArch core, and two
    /// machines: `desk` sorts RetroArch's saves by core (RetroArch's default), `deck` is
    /// RetroDECK, which sorts them by content folder.
    fn setup(root: &Path) -> Settings {
        let text = format!(
            "library_root = \"{lib}\"\nviews_root = \"{views}\"\ndatabase = \"{db}\"\n\
             undo_dir = \"{undo}\"\n\
             [systems]\ngenesis = {{ folder = \"Genesis\" }}\nsegacd = {{ folder = \"SegaCD\" }}\n\
             [uses]\ngenesis = [\"ra-genesis_plus_gx\"]\nsegacd = [\"ra-genesis_plus_gx\"]\n",
            lib = root.join("lib").display(),
            views = root.join("views").display(),
            db = root.join("db.sqlite").display(),
            undo = root.join("undo").display(),
        );
        let mut settings: Settings = toml::from_str(&text).unwrap();
        for (name, platform, home) in [
            ("desk", "es-de", "/home/deus"),
            ("deck", "retrodeck", "/home/deck"),
        ] {
            let mut node = Node::new(platform, Default::default());
            node.home = Some(home.into());
            node.uses = settings.uses.clone();
            settings.nodes.insert(name.into(), node);
        }
        settings.validate().unwrap();
        for game in [
            "Genesis/Sonic (USA).md",
            "Genesis/Shared Name (USA).md",
            "SegaCD/Sonic CD (USA).chd",
            "SegaCD/Shared Name (USA).chd",
        ] {
            write(&root.join("lib").join(game), "rom");
        }
        settings
    }

    /// The view folder (relative to the view root) a machine keeps one system's saves in.
    fn save_folder(settings: &Settings, node: &str, system: &str) -> PathBuf {
        let index = crate::emulators::index(settings).unwrap();
        let (dirs, _) = save_dirs(settings, node, &crate::emulators::by_id(&index)).unwrap();
        dirs.iter()
            .find(|d| d.sources.iter().any(|s| s.system == system))
            .unwrap()
            .view_path()
    }

    #[test]
    fn saves_are_projected_in_each_machines_shape_and_new_ones_reach_every_machine() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let settings = setup(root);
        let mut catalog = Catalog::open(&settings.database).unwrap();
        let saves = root.join("lib/saves");
        write(&saves.join("genesis/Sonic (USA).srm"), "genesis save");

        let desk = settings.view_root("desk");
        let deck = settings.view_root("deck");
        let desk_core = save_folder(&settings, "desk", "genesis");
        assert_eq!(
            desk_core,
            save_folder(&settings, "desk", "segacd"),
            "sorted by core: one folder for both systems"
        );
        assert!(desk_core.ends_with(".config/retroarch/saves/Genesis Plus GX"));
        let deck_genesis = save_folder(&settings, "deck", "genesis");
        let deck_segacd = save_folder(&settings, "deck", "segacd");
        assert_ne!(deck_genesis, deck_segacd, "sorted by content: one each");

        // The library's save shows up on both machines, as links to the one file.
        crate::monitor::refresh(&settings, &mut catalog).unwrap();
        let on_desk = desk.join(&desk_core).join("Sonic (USA).srm");
        let on_deck = deck.join(&deck_genesis).join("Sonic (USA).srm");
        assert_eq!(read(&on_desk), "genesis save");
        assert_eq!(std::fs::metadata(&on_deck).unwrap().nlink(), 3);

        // An emulator rewriting the save in place writes straight through to the library.
        std::fs::write(&on_desk, "played more").unwrap();
        assert_eq!(read(&saves.join("genesis/Sonic (USA).srm")), "played more");
        assert_eq!(read(&on_deck), "played more");

        // A new Sega CD save on desk sits in the core's shared folder; its name says which
        // system it is, so it goes to Sega CD's library folder and on to deck's Sega CD folder.
        write(&desk.join(&desk_core).join("Sonic CD (USA).srm"), "cd save");
        // Saves named after no game, or after games on both systems, stay where they are.
        write(
            &desk.join(&desk_core).join("Shared Name (USA).srm"),
            "which?",
        );
        write(&desk.join(&desk_core).join("Nothing.srm"), "unknown");
        crate::monitor::refresh(&settings, &mut catalog).unwrap();
        assert_eq!(read(&saves.join("segacd/Sonic CD (USA).srm")), "cd save");
        assert!(!saves.join("genesis/Sonic CD (USA).srm").exists());
        let cd_on_desk = desk.join(&desk_core).join("Sonic CD (USA).srm");
        assert_eq!(std::fs::metadata(&cd_on_desk).unwrap().nlink(), 3);
        assert_eq!(
            read(&deck.join(&deck_segacd).join("Sonic CD (USA).srm")),
            "cd save"
        );
        assert!(!deck.join(&deck_genesis).join("Sonic CD (USA).srm").exists());
        for name in ["Shared Name (USA).srm", "Nothing.srm"] {
            let left = desk.join(&desk_core).join(name);
            assert_eq!(std::fs::metadata(&left).unwrap().nlink(), 1, "{name}");
            assert!(!saves.join("genesis").join(name).exists());
            assert!(!saves.join("segacd").join(name).exists());
        }
        let alerts = catalog.alerts().unwrap();
        assert_eq!(
            alerts.len(),
            2,
            "the two unplaced saves are alerts: {alerts:?}"
        );

        // A second pass changes nothing.
        let index = crate::emulators::index(&settings).unwrap();
        let emulators = crate::emulators::by_id(&index);
        let plan = plan_ingest(&settings, &catalog, "desk", &emulators).unwrap();
        assert!(plan.items.is_empty(), "{:?}", plan.items);
        assert_eq!(plan.problems.len(), 2, "{:?}", plan.problems);
    }

    #[test]
    fn existing_library_saves_are_never_overwritten_by_a_machines_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let settings = setup(root);
        let mut catalog = Catalog::open(&settings.database).unwrap();
        let index = crate::emulators::index(&settings).unwrap();
        let emulators = crate::emulators::by_id(&index);
        let saves = root.join("lib/saves");
        let deck = settings.view_root("deck");
        let deck_genesis = deck.join(save_folder(&settings, "deck", "genesis"));

        // The machine had its own copy before the library's was linked in. The same bytes: the
        // copy just becomes a link. Different bytes: both are kept and nothing is replaced.
        write(&saves.join("genesis/Sonic (USA).srm"), "library");
        write(&deck_genesis.join("Sonic (USA).srm"), "library");
        write(&saves.join("genesis/Shared Name (USA).srm"), "library");
        write(&deck_genesis.join("Shared Name (USA).srm"), "machine");
        let plan = plan_ingest(&settings, &catalog, "deck", &emulators).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].action, IngestAction::Relink);
        assert!(
            plan.problems[0].contains("already has a different"),
            "{:?}",
            plan.problems
        );
        apply_ingest(&settings, &mut catalog, "deck", &plan).unwrap();
        assert_eq!(
            std::fs::metadata(deck_genesis.join("Sonic (USA).srm"))
                .unwrap()
                .nlink(),
            2
        );
        assert_eq!(
            read(&saves.join("genesis/Shared Name (USA).srm")),
            "library"
        );
        assert_eq!(read(&deck_genesis.join("Shared Name (USA).srm")), "machine");

        // An emulator that saves by writing a new file replaces RomServer's link: that's the
        // newest save, so it replaces the library's, and the old one is kept in the undo folder.
        let linked = deck_genesis.join("Sonic (USA).srm");
        std::fs::remove_file(&linked).unwrap();
        write(&linked, "newer");
        let plan = plan_ingest(&settings, &catalog, "deck", &emulators).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].action, IngestAction::Update);
        apply_ingest(&settings, &mut catalog, "deck", &plan).unwrap();
        assert_eq!(read(&saves.join("genesis/Sonic (USA).srm")), "newer");
        assert_eq!(std::fs::metadata(&linked).unwrap().nlink(), 2);
        let kept: Vec<PathBuf> = walk(&root.join("undo"))
            .unwrap()
            .into_iter()
            .filter(|p| p.ends_with("saves/genesis/Sonic (USA).srm"))
            .collect();
        assert_eq!(kept.len(), 1, "old save kept in the undo folder");
        assert_eq!(read(&root.join("undo").join(&kept[0])), "library");
    }

    #[test]
    fn library_files_with_one_name_keep_the_first_and_warn() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let settings = setup(root);
        let saves = root.join("lib/saves");
        // Sorted by core, both systems share one folder, so equal names collide.
        write(&saves.join("genesis/Same.srm"), "genesis");
        write(&saves.join("segacd/same.srm"), "segacd");
        let index = crate::emulators::index(&settings).unwrap();
        let (dirs, _) = save_dirs(&settings, "desk", &crate::emulators::by_id(&index)).unwrap();
        let mut warnings = Vec::new();
        let links = save_links(&dirs, &mut warnings).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].src, saves.join("genesis/Same.srm"));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[test]
    fn save_names_may_contain_dots() {
        assert_eq!(
            name_candidates("Super Mario Bros. 3.srm"),
            ["super mario bros. 3", "super mario bros"]
        );
        assert_eq!(name_candidates("Game.state1.png"), ["game.state1", "game"]);
    }
}
