//! End-to-end checks against a small fake library in a temp dir.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::catalog::Catalog;
use crate::project::{Link, project_system};
use crate::scan::{Game, scan_system};
use crate::settings::{Frontend, MultiDisc};
use crate::sync;

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// A flat PSX folder in canonical form, plus one cartridge-style system.
fn library(root: &Path) -> PathBuf {
    let psx = root.join("PSX");
    write(
        &psx.join("FF7 (USA).m3u"),
        "FF7 (USA) (Disc 1)\nFF7 (USA) (Disc 2)\n",
    );
    write(
        &psx.join("FF7 (USA) (Disc 1)"),
        "FILE \"FF7 (USA) (Disc 1).bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n",
    );
    write(
        &psx.join("FF7 (USA) (Disc 2)"),
        "FILE \"FF7 (USA) (Disc 2).bin\" BINARY\n  TRACK 01 MODE2/2352\n",
    );
    write(&psx.join("FF7 (USA) (Disc 1).bin"), "disc1");
    write(&psx.join("FF7 (USA) (Disc 2).bin"), "disc2");
    write(
        &psx.join("Lemmings (USA).cue"),
        "FILE \"Lemmings (USA) (Track 1).bin\" BINARY\nFILE \"Lemmings (USA) (Track 2).bin\" BINARY\n",
    );
    write(&psx.join("Lemmings (USA) (Track 1).bin"), "t1");
    write(&psx.join("Lemmings (USA) (Track 2).bin"), "t2");
    write(&psx.join("FF7 (USA).srm"), "save");
    write(&psx.join("notes.txt"), "x");
    // Broken set: playlist names a disc that doesn't exist.
    write(
        &psx.join("Broken (USA).m3u"),
        "Broken (USA) (Disc 1)\nBroken (USA) (Disc 2)\n",
    );
    write(
        &psx.join("Broken (USA) (Disc 1)"),
        "FILE \"Broken (USA) (Disc 1).bin\" BINARY\n",
    );
    write(&psx.join("Broken (USA) (Disc 1).bin"), "b1");
    psx
}

fn names(scan: &crate::scan::SystemScan) -> Vec<&str> {
    scan.games.iter().map(Game::name).collect()
}

#[test]
fn scan_groups_sets_and_skips_saves() {
    let tmp = tempfile::tempdir().unwrap();
    let psx = library(tmp.path());
    let scan = scan_system(&psx).unwrap();
    // Broken playlist is reported, its existing disc still shows up as a single game.
    assert_eq!(
        names(&scan),
        vec!["Broken (USA) (Disc 1)", "FF7 (USA)", "Lemmings (USA)"]
    );
    assert_eq!(scan.issues.len(), 1, "{:?}", scan.issues);
    assert!(scan.issues[0].contains("Broken (USA) (Disc 2): missing"));
    assert_eq!(scan.saves, vec![PathBuf::from("FF7 (USA).srm")]);
    assert_eq!(scan.ignored, vec![PathBuf::from("notes.txt")]);
    let Game::MultiDisc { discs, .. } = &scan.games[1] else {
        panic!()
    };
    assert_eq!(discs.len(), 2);
    assert_eq!(
        discs[0].tracks,
        vec![PathBuf::from("FF7 (USA) (Disc 1).bin")]
    );
}

fn dsts(links: &[Link]) -> Vec<String> {
    let mut v: Vec<String> = links.iter().map(|l| l.dst.display().to_string()).collect();
    v.sort();
    v
}

#[test]
fn multi_disc_modes() {
    let tmp = tempfile::tempdir().unwrap();
    let psx = library(tmp.path());
    let scan = scan_system(&psx).unwrap();
    let proj = |mode| {
        dsts(
            &project_system(Frontend::EsDe, "psx", &psx, &scan, mode)
                .unwrap()
                .links,
        )
    };

    let m3u = proj(MultiDisc::M3u);
    assert!(m3u.contains(&"psx/FF7 (USA).m3u".into()));
    assert!(m3u.contains(&"psx/FF7 (USA) (Disc 1)".into()));
    // A lone extensionless cue regains .cue so the frontend lists it.
    assert!(m3u.contains(&"psx/Broken (USA) (Disc 1).cue".into()));
    assert!(
        !m3u.iter()
            .any(|d| d.ends_with(".srm") || d.ends_with(".txt"))
    );

    let discs = proj(MultiDisc::Discs);
    assert!(!discs.iter().any(|d| d.contains("FF7 (USA).m3u")));
    assert!(discs.contains(&"psx/FF7 (USA) (Disc 1).cue".into()));
    assert!(discs.contains(&"psx/FF7 (USA) (Disc 1).bin".into()));

    let dir = proj(MultiDisc::M3uDirectory);
    assert!(dir.contains(&"psx/FF7 (USA).m3u/FF7 (USA).m3u".into()));
    assert!(dir.contains(&"psx/FF7 (USA).m3u/FF7 (USA) (Disc 2).bin".into()));
}

struct Env {
    _tmp: tempfile::TempDir,
    psx: PathBuf,
    view: PathBuf,
    catalog: Catalog,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let psx = library(tmp.path());
    let view = tmp.path().join("views/desktop");
    let catalog = Catalog::open(&tmp.path().join("config/romserver.sqlite")).unwrap();
    Env {
        psx,
        view,
        catalog,
        _tmp: tmp,
    }
}

fn sync_mode(e: &mut Env, mode: MultiDisc) -> sync::Plan {
    let scan = scan_system(&e.psx).unwrap();
    let links = project_system(Frontend::EsDe, "psx", &e.psx, &scan, mode)
        .unwrap()
        .links;
    let owned: BTreeMap<_, _> = e.catalog.owned("desktop").unwrap();
    let plan = sync::plan(&e.view, links, &owned).unwrap();
    sync::apply(&mut e.catalog, "desktop", &e.view, &plan).unwrap();
    plan
}

#[test]
fn apply_links_same_inode_and_is_idempotent() {
    let mut e = env();
    let first = sync_mode(&mut e, MultiDisc::M3u);
    assert!(first.conflicts.is_empty());
    let src = fs::metadata(e.psx.join("FF7 (USA) (Disc 1)")).unwrap();
    let dst = fs::metadata(e.view.join("psx/FF7 (USA) (Disc 1)")).unwrap();
    assert_eq!((src.dev(), src.ino()), (dst.dev(), dst.ino()));
    assert!(sync_mode(&mut e, MultiDisc::M3u).is_noop());
}

#[test]
fn switching_modes_removes_old_names_and_adds_new() {
    let mut e = env();
    sync_mode(&mut e, MultiDisc::M3u);
    let plan = sync_mode(&mut e, MultiDisc::Discs);
    assert!(plan.remove.contains(&PathBuf::from("psx/FF7 (USA).m3u")));
    assert!(!e.view.join("psx/FF7 (USA).m3u").exists());
    assert!(!e.view.join("psx/FF7 (USA) (Disc 1)").exists());
    assert!(e.view.join("psx/FF7 (USA) (Disc 1).cue").exists());

    sync_mode(&mut e, MultiDisc::M3uDirectory);
    assert!(e.view.join("psx/FF7 (USA).m3u/FF7 (USA).m3u").is_file());
    assert!(!e.view.join("psx/FF7 (USA) (Disc 1).cue").exists());
}

#[test]
fn foreign_files_and_last_copies_are_never_touched() {
    let mut e = env();
    // Something not created by RomServer sits where a link should go.
    write(&e.view.join("psx/Lemmings (USA).cue"), "user's own file");
    let plan = sync_mode(&mut e, MultiDisc::M3u);
    assert_eq!(plan.conflicts.len(), 1);
    assert_eq!(
        fs::read_to_string(e.view.join("psx/Lemmings (USA).cue")).unwrap(),
        "user's own file"
    );

    // Library loses a game: the view link is now the only copy, so it must survive.
    fs::remove_file(e.psx.join("FF7 (USA) (Disc 2).bin")).unwrap();
    fs::remove_file(e.psx.join("FF7 (USA) (Disc 2)")).unwrap();
    fs::write(e.psx.join("FF7 (USA).m3u"), "FF7 (USA) (Disc 1)\n").unwrap();
    let plan = sync_mode(&mut e, MultiDisc::M3u);
    assert!(plan.conflicts.iter().any(|c| c.contains("last copy")));
    assert_eq!(
        fs::read_to_string(e.view.join("psx/FF7 (USA) (Disc 2).bin")).unwrap(),
        "disc2"
    );
}

#[test]
fn regenerated_m3u_is_relinked_not_edited() {
    let mut e = env();
    sync_mode(&mut e, MultiDisc::M3u);
    let old = fs::metadata(e.view.join("psx/FF7 (USA).m3u"))
        .unwrap()
        .ino();
    // Regenerate the playlist as a new file, as Prep Folders does.
    let m3u = e.psx.join("FF7 (USA).m3u");
    fs::remove_file(&m3u).unwrap();
    fs::write(&m3u, "FF7 (USA) (Disc 1)\nFF7 (USA) (Disc 2)\n").unwrap();
    let plan = sync_mode(&mut e, MultiDisc::M3u);
    assert_eq!(plan.replace.len(), 1);
    let new = fs::metadata(e.view.join("psx/FF7 (USA).m3u")).unwrap();
    assert_ne!(new.ino(), old);
    assert_eq!(new.ino(), fs::metadata(&m3u).unwrap().ino());
}
