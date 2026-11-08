//! Save monitor: finds files in machines' views that RomServer didn't create. A save beside a
//! ROM means that machine's RetroArch writes saves into the content folder (so the save stays in
//! that one view); `adopt` moves it into the shared save folder. Also answers "my save didn't
//! carry over" by searching every place a save could have gone.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::catalog::{Alert, AlertKind, Catalog};
use crate::emulators::{EmulatorInfo, Kind};
use crate::platforms::{self, VIEW_ROMS};
use crate::saves;
use crate::scan::{self, is_save_name};
use crate::settings::{Settings, is_plain_relative};

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Every file in every view that isn't recorded as RomServer's.
pub fn scan_views(settings: &Settings, catalog: &Catalog) -> Result<Vec<Alert>> {
    let mut found = Vec::new();
    for node in settings.nodes.keys() {
        let root = settings.view_root(node);
        if !root.is_dir() {
            continue;
        }
        let owned = catalog.owned(node)?;
        let mut files = Vec::new();
        walk(&root, Path::new(""), &mut files)?;
        for path in files {
            if owned.contains_key(&path) {
                continue;
            }
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            let kind = if name.as_deref().is_some_and(is_save_name) {
                AlertKind::Save
            } else {
                AlertKind::Other
            };
            found.push(Alert {
                node: node.clone(),
                path,
                kind,
                first_seen: 0,
                dismissed: false,
            });
        }
    }
    Ok(found)
}

fn walk(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let dir = root.join(rel);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        // A folder removed mid-walk (an apply pruning it) isn't an error.
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    for entry in entries {
        let entry = entry?;
        let path = rel.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            walk(root, &path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// Runs the monitor once: brings saves machines created into the library, brings every view's
/// save and state links up to date (so a save made on one machine reaches the others), then
/// stores what's left as alerts. Returns how many save alerts are open. A machine whose saves
/// can't be handled is reported and the others still run.
pub fn refresh(settings: &Settings, catalog: &mut Catalog) -> Result<usize> {
    let index = crate::emulators::index(settings)?;
    let emulators = crate::emulators::by_id(&index);
    let nodes: Vec<String> = settings.nodes.keys().cloned().collect();
    // Every machine's new saves first, so the second pass links them everywhere.
    for node in &nodes {
        let result = saves::plan_ingest(settings, catalog, node, &emulators).and_then(|plan| {
            for problem in &plan.problems {
                println!("[monitor] {node}: {problem}");
            }
            saves::apply_ingest(settings, catalog, node, &plan)
        });
        match result {
            Ok(0) => {}
            Ok(n) => println!("[monitor] {node}: {n} new save file(s) brought into the library"),
            Err(e) => println!("[monitor] {node}: saves not brought in: {e:#}"),
        }
    }
    for node in &nodes {
        if let Err(e) = saves::sync_view(settings, catalog, node, &emulators) {
            println!("[monitor] {node}: save links not updated: {e:#}");
        }
    }
    let found = scan_views(settings, catalog)?;
    catalog.sync_alerts(&found, now())?;
    Ok(found.iter().filter(|a| a.kind == AlertKind::Save).count())
}

/// The system a view path belongs to, from its `roms/<folder>/` prefix.
fn system_of<'a>(settings: &'a Settings, node: &str, path: &Path) -> Option<&'a str> {
    let node_settings = settings.nodes.get(node)?;
    let platforms = platforms::bundled();
    let layout = platforms.layout(platforms.platform(&node_settings.platform).ok()?);
    let mut parts = path.components();
    if parts.next()?.as_os_str() != VIEW_ROMS {
        return None;
    }
    let folder = parts.next()?.as_os_str().to_str()?;
    settings
        .node_systems(node_settings)
        .into_iter()
        .find(|s| layout.folder(s) == folder)
}

/// Save folders a file found in a view could belong to: those of the emulators the machine has
/// for that system, RetroArch cores first (they're the ones that save beside the ROM).
pub fn adopt_candidates(
    settings: &Settings,
    node: &str,
    path: &Path,
    emulators: &std::collections::HashMap<&str, &EmulatorInfo>,
) -> Vec<String> {
    let Some(system) = system_of(settings, node, path) else {
        return Vec::new();
    };
    let Some(ids) = settings.nodes.get(node).and_then(|n| n.uses.get(system)) else {
        return Vec::new();
    };
    let mut ordered: Vec<&String> = ids.iter().collect();
    ordered.sort_by_key(|id| {
        !emulators
            .get(id.as_str())
            .is_some_and(|e| e.kind == Kind::RetroArch)
    });
    let mut out: Vec<String> = Vec::new();
    for id in ordered {
        let folder = settings.save_folder(system, id);
        if !out.contains(&folder) {
            out.push(folder);
        }
    }
    out
}

/// Moves a save out of a view into a shared save folder. Never overwrites: if the folder
/// already has a file with that name, nothing moves and the user decides.
pub fn adopt(settings: &Settings, node: &str, path: &Path, folder: &str) -> Result<PathBuf> {
    if !is_plain_relative(path)
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        bail!("bad path {}", path.display());
    }
    if !crate::settings::is_folder_name(folder) {
        bail!("bad save folder {folder:?}");
    }
    let source = settings.view_root(node).join(path);
    let meta = std::fs::symlink_metadata(&source)
        .with_context(|| format!("{} is gone", source.display()))?;
    if !meta.is_file() {
        bail!("{} isn't a regular file", source.display());
    }
    let name = path.file_name().context("no file name")?;
    let dir = settings.saves_root().join(folder);
    let target = dir.join(name);
    if std::fs::symlink_metadata(&target).is_ok() {
        bail!(
            "{} already exists. Compare the two and keep the one you want; nothing was moved.",
            target.display()
        );
    }
    crate::ownership::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    match std::fs::rename(&source, &target) {
        Ok(()) => {}
        // Different filesystems: copy, check, then remove the original.
        Err(e) if e.raw_os_error() == Some(18) => {
            crate::ownership::copy(&source, &target)
                .with_context(|| format!("copy to {}", target.display()))?;
            if std::fs::metadata(&target)?.len() != meta.len() {
                let _ = std::fs::remove_file(&target);
                bail!(
                    "copy of {} came out the wrong size; original kept",
                    source.display()
                );
            }
            std::fs::remove_file(&source)
                .with_context(|| format!("remove {}", source.display()))?;
        }
        Err(e) => return Err(e).with_context(|| format!("move to {}", target.display())),
    }
    Ok(target)
}

/// Where a save for one game turned up.
pub struct CheckReport {
    /// The game name searched for (what RetroArch names the save after).
    pub stem: String,
    /// The save folders this machine's emulators use.
    pub expected_folders: Vec<String>,
    /// Matches under the shared save folders: (folder, file).
    pub in_save_folders: Vec<(String, PathBuf)>,
    /// Matches sitting in machines' views: (machine, path in view).
    pub in_views: Vec<(String, PathBuf)>,
}

impl CheckReport {
    pub fn found_expected(&self) -> bool {
        self.in_save_folders
            .iter()
            .any(|(f, _)| self.expected_folders.contains(f))
    }
}

/// Searches every save folder and every view for saves named after `stem`.
pub fn check(
    settings: &Settings,
    catalog: &Catalog,
    node: &str,
    stem: &str,
) -> Result<CheckReport> {
    let node_settings = settings.node(node)?;
    let expected_folders: Vec<String> = node_settings
        .uses
        .iter()
        .flat_map(|(system, ids)| ids.iter().map(|id| settings.save_folder(system, id)))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let matches = |name: &str| is_save_name(name) && scan::stem(name).eq_ignore_ascii_case(stem);

    let mut in_save_folders = Vec::new();
    let saves_root = settings.saves_root();
    if let Ok(folders) = std::fs::read_dir(&saves_root) {
        for folder in folders.flatten() {
            if !folder.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let folder_name = folder.file_name().to_string_lossy().into_owned();
            let mut files = Vec::new();
            walk(&folder.path(), Path::new(""), &mut files)?;
            for f in files {
                if f.file_name().is_some_and(|n| matches(&n.to_string_lossy())) {
                    in_save_folders.push((folder_name.clone(), f));
                }
            }
        }
    }
    let in_views = scan_views(settings, catalog)?
        .into_iter()
        .filter(|a| {
            a.path
                .file_name()
                .is_some_and(|n| matches(&n.to_string_lossy()))
        })
        .map(|a| (a.node, a.path))
        .collect();
    Ok(CheckReport {
        stem: stem.to_string(),
        expected_folders,
        in_save_folders,
        in_views,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Link;
    use crate::settings::Node;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn finds_foreign_files_adopts_saves_and_checks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let text = format!(
            "library_root = \"{lib}\"\nviews_root = \"{views}\"\ndatabase = \"{db}\"\n\
             [systems]\nsnes = {{ folder = \"SNES\" }}\n\
             [uses]\nsnes = [\"ra-snes9x\"]\n\
             [save_folders.snes]\nra-snes9x = \"snes-saves\"\n",
            lib = root.join("lib").display(),
            views = root.join("views").display(),
            db = root.join("db.sqlite").display(),
        );
        let mut settings: Settings = toml::from_str(&text).unwrap();
        let mut node = Node::new("es-de", Default::default());
        node.uses.insert("snes".into(), vec!["ra-snes9x".into()]);
        settings.nodes.insert("deck".into(), node);
        settings.validate().unwrap();

        let rom = root.join("lib/SNES/Chrono Trigger (USA).sfc");
        write(&rom, "rom");
        let mut catalog = Catalog::open(&settings.database).unwrap();
        let view = settings.view_root("deck");
        let link = Link {
            dst: "roms/snes/Chrono Trigger (USA).sfc".into(),
            src: rom.clone(),
        };
        let plan = crate::sync::plan(&view, vec![link], &catalog.owned("deck").unwrap()).unwrap();
        crate::sync::apply(&mut catalog, "deck", &view, &plan).unwrap();

        // The machine writes a save beside the ROM, and its scraper drops a gamelist.
        write(&view.join("roms/snes/Chrono Trigger (USA).srm"), "save");
        write(&view.join("roms/snes/gamelist.xml"), "<x/>");
        assert_eq!(refresh(&settings, &mut catalog).unwrap(), 1);
        let alerts = catalog.alerts().unwrap();
        assert_eq!(alerts.len(), 2, "the linked ROM isn't an alert: {alerts:?}");
        let save = alerts.iter().find(|a| a.kind == AlertKind::Save).unwrap();

        let snes9x = EmulatorInfo {
            id: "ra-snes9x".into(),
            name: "Snes9x".into(),
            kind: Kind::RetroArch,
            corename: Some("Snes9x".into()),
        };
        let index = std::collections::HashMap::from([("ra-snes9x", &snes9x)]);
        assert_eq!(
            adopt_candidates(&settings, "deck", &save.path, &index),
            ["snes-saves"]
        );

        // Not carried over yet: it's only in the view.
        let report = check(&settings, &catalog, "deck", "chrono trigger (usa)").unwrap();
        assert!(!report.found_expected());
        assert_eq!(report.in_views.len(), 1);

        let target = adopt(&settings, "deck", &save.path, "snes-saves").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "save");
        assert!(!view.join(&save.path).exists());
        refresh(&settings, &mut catalog).unwrap();
        assert_eq!(
            catalog.alerts().unwrap().len(),
            1,
            "adopted save no longer an alert"
        );

        let report = check(&settings, &catalog, "deck", "Chrono Trigger (USA)").unwrap();
        assert!(report.found_expected());
        assert!(report.in_views.is_empty());

        // Never overwrites.
        write(&view.join("roms/snes/Chrono Trigger (USA).srm"), "newer");
        assert!(adopt(&settings, "deck", &save.path, "snes-saves").is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "save");
        assert!(adopt(&settings, "deck", Path::new("../escape.srm"), "snes-saves").is_err());
    }
}
