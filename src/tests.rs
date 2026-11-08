//! End-to-end checks against a small fake library in a temp dir.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::catalog::Catalog;
use crate::project::{Link, project_system};
use crate::scan::{Game, scan_system};
use crate::settings::MultiDisc;
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
            &project_system(Path::new("psx"), &psx, &scan, mode)
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

/// Media in the library's own layout: one folder per game, disc sets included, with art that
/// differs per disc in `disc-N/`.
fn canonical_media(root: &Path) -> PathBuf {
    let media = root.join("media/psx");
    write(&media.join("FF7 (USA)/box-front.png"), "c1");
    write(&media.join("FF7 (USA)/screenshot.png"), "s1");
    write(&media.join("FF7 (USA)/disc-2/box-front.png"), "c2");
    write(&media.join("FF7 (USA)/video.mp4"), "v");
    write(&media.join("Lemmings (USA)/box-front.png"), "l");
    write(&media.join("Lemmings (USA)/logo.png"), "logo");
    write(&media.join("Unrelated/box-front.png"), "x");
    media
}

fn media_dsts(psx: &Path, media: &Path, layout: &str, mode: MultiDisc) -> Vec<String> {
    let scan = scan_system(psx).unwrap();
    let index = crate::media::index_system(media).unwrap();
    let layout = &crate::platforms::bundled().layouts[layout];
    let p = project_system(&layout.rom_dir("psx"), psx, &scan, mode).unwrap();
    dsts(&crate::media::media_links(
        &index,
        media,
        &layout.media_base("psx"),
        &layout.media,
        &p.entries,
    ))
}

#[test]
fn es_de_views_get_their_own_media_tree_under_the_listed_names() {
    let tmp = tempfile::tempdir().unwrap();
    let psx = library(tmp.path());
    let media = canonical_media(tmp.path());
    let m3u = media_dsts(&psx, &media, "es-de", MultiDisc::M3u);
    assert_eq!(
        m3u,
        vec![
            "downloaded_media/psx/covers/FF7 (USA).png",
            "downloaded_media/psx/covers/Lemmings (USA).png",
            "downloaded_media/psx/marquees/Lemmings (USA).png",
            "downloaded_media/psx/screenshots/FF7 (USA).png",
            "downloaded_media/psx/videos/FF7 (USA).mp4",
        ]
    );
    // Per disc, a disc's own art wins; everything else is the game's.
    let discs = media_dsts(&psx, &media, "es-de", MultiDisc::Discs);
    let src = |dst: &str| {
        let scan = scan_system(&psx).unwrap();
        let layout = &crate::platforms::bundled().layouts["es-de"];
        let p = project_system(&layout.rom_dir("psx"), &psx, &scan, MultiDisc::Discs).unwrap();
        let index = crate::media::index_system(&media).unwrap();
        crate::media::media_links(
            &index,
            &media,
            &layout.media_base("psx"),
            &layout.media,
            &p.entries,
        )
        .into_iter()
        .find(|l| l.dst == Path::new(dst))
        .map(|l| fs::read_to_string(l.src).unwrap())
    };
    assert_eq!(
        src("downloaded_media/psx/covers/FF7 (USA) (Disc 2).png").as_deref(),
        Some("c2")
    );
    assert_eq!(
        src("downloaded_media/psx/covers/FF7 (USA) (Disc 1).png").as_deref(),
        Some("c1")
    );
    assert!(discs.contains(&"downloaded_media/psx/videos/FF7 (USA) (Disc 2).mp4".into()));
    // ES-DE lists the Game.m3u folder and names its media after the folder's stem.
    assert_eq!(
        media_dsts(&psx, &media, "es-de", MultiDisc::M3uDirectory),
        m3u
    );
}

#[test]
fn batocera_views_get_media_beside_the_roms_in_its_naming() {
    let tmp = tempfile::tempdir().unwrap();
    let psx = library(tmp.path());
    let media = canonical_media(tmp.path());
    let links = media_dsts(&psx, &media, "batocera", MultiDisc::M3u);
    assert_eq!(
        links,
        vec![
            "roms/psx/images/FF7 (USA)-image.png",
            "roms/psx/images/FF7 (USA)-thumb.png",
            "roms/psx/images/Lemmings (USA)-marquee.png",
            "roms/psx/images/Lemmings (USA)-thumb.png",
            "roms/psx/images/Lemmings (USA)-wheel.png",
            "roms/psx/videos/FF7 (USA)-video.mp4",
        ]
    );
}

#[test]
fn existing_media_is_imported_where_recognised_and_asked_about_otherwise() {
    use crate::media::{Decision, apply_existing, plan_existing};
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let psx = library(root);
    let media = root.join("media/psx");
    // An ES-DE tree in the media folder.
    write(&media.join("covers/FF7 (USA).png"), "cover");
    write(&media.join("marquees/Lemmings (USA).png"), "logo");
    write(&media.join("covers/Unknown Game.png"), "?");
    // Already in the library's layout, plus an ES-DE file that would clash with it.
    write(&media.join("Lemmings (USA)/box-front.png"), "kept");
    write(&media.join("covers/Lemmings (USA).png"), "clash");
    // In the ROM folder: a set scraped once per disc in Batocera's naming, and an old Images/.
    // Disc 1's cover is the set's cover again (a duplicate); disc 2's differs (its own art).
    write(&psx.join("images/FF7 (USA) (Disc 1)-thumb.png"), "cover");
    write(&psx.join("images/FF7 (USA) (Disc 2)-thumb.png"), "disc 2");
    // No set-level image: disc 1's becomes the game's, disc 2's identical one is a duplicate.
    write(&psx.join("images/FF7 (USA) (Disc 1)-image.png"), "shot");
    write(&psx.join("images/FF7 (USA) (Disc 2)-image.png"), "shot");
    write(&psx.join("Images/Lemmings (USA).jpg"), "old");
    // A folder-based game's own data (PS3 style) and stray images: never scraped media.
    write(&psx.join("Some Game.ps3/PS3_GAME/ICON0.PNG"), "icon");
    write(&psx.join("Some Game.ps3/PS3_GAME/PIC1.PNG"), "backdrop");
    write(&psx.join("Some Game.ps3/extras/poster.png"), "poster");
    let settings: crate::settings::Settings = toml::from_str(&format!(
        "library_root = \"{r}\"\nviews_root = \"{r}/views\"\ndatabase = \"{r}/db\"\n\
         undo_dir = \"{r}/undo\"\n[systems]\npsx = {{ folder = \"PSX\" }}\n",
        r = root.display()
    ))
    .unwrap();
    let inode = fs::metadata(media.join("covers/FF7 (USA).png"))
        .unwrap()
        .ino();

    let plan = plan_existing(&settings, "psx").unwrap();
    let auto: Vec<String> = plan
        .auto
        .iter()
        .map(|e| e.from.display().to_string())
        .collect();
    assert_eq!(auto.len(), 4, "{auto:?}");
    assert_eq!(plan.duplicates.len(), 2, "{:?}", plan.duplicates);
    assert_eq!(plan.needs_input.len(), 3, "{:?}", plan.needs_input);
    let find = |name: &str| {
        plan.needs_input
            .iter()
            .find(|e| e.from.ends_with(name))
            .unwrap()
            .clone()
    };
    let old = find("Images/Lemmings (USA).jpg");
    assert_eq!(old.game.as_deref(), Some("Lemmings (USA)"));
    assert_eq!(old.kind, None, "named after a game, kind unknown");
    let unknown = find("Unknown Game.png");
    assert_eq!(unknown.game, None);
    let clash = find("covers/Lemmings (USA).png");
    assert!(
        clash.reason.contains("already has a different box-front"),
        "{}",
        clash.reason
    );

    let decided = vec![
        (
            old,
            Decision::Import {
                game: "Lemmings (USA)".into(),
                kind: "title-screen".into(),
            },
        ),
        (unknown, Decision::Discard),
        (clash, Decision::Leave),
    ];
    let (imported, discarded, _) =
        apply_existing(&settings, "psx", &plan.auto, &plan.duplicates, &decided).unwrap();
    assert_eq!((imported, discarded), (5, 3));

    let cover = media.join("FF7 (USA)/box-front.png");
    assert_eq!(
        fs::metadata(&cover).unwrap().ino(),
        inode,
        "a rename, not a copy"
    );
    assert_eq!(
        fs::read_to_string(media.join("Lemmings (USA)/logo.png")).unwrap(),
        "logo"
    );
    assert_eq!(
        fs::read_to_string(media.join("Lemmings (USA)/box-front.png")).unwrap(),
        "kept"
    );
    assert_eq!(
        fs::read_to_string(media.join("Lemmings (USA)/title-screen.jpg")).unwrap(),
        "old"
    );
    // One folder for the set; only the art that differs per disc is kept per disc.
    assert_eq!(
        fs::read_to_string(media.join("FF7 (USA)/disc-2/box-front.png")).unwrap(),
        "disc 2"
    );
    // Batocera's -image is read as its first type, the mix image.
    assert_eq!(
        fs::read_to_string(media.join("FF7 (USA)/mix-image.png")).unwrap(),
        "shot"
    );
    assert!(!media.join("FF7 (USA)/disc-1").exists());
    assert!(!media.join("FF7 (USA)/disc-2/mix-image.png").exists());
    assert!(
        !media.join("FF7 (USA) (Disc 1)").exists() && !media.join("FF7 (USA) (Disc 2)").exists()
    );
    assert!(
        media.join("covers/Lemmings (USA).png").exists(),
        "left where it was"
    );
    assert!(!media.join("covers/Unknown Game.png").exists(), "discarded");
    assert!(
        root.join("undo").read_dir().unwrap().count() >= 2,
        "log and discarded copy"
    );
    assert!(
        !media.join("marquees").exists() && !psx.join("Images").exists(),
        "emptied"
    );
    assert!(psx.join("FF7 (USA).m3u").exists(), "ROMs untouched");
    assert!(
        psx.join("Some Game.ps3/PS3_GAME/ICON0.PNG").exists(),
        "game data untouched"
    );
    assert!(psx.join("Some Game.ps3/extras/poster.png").exists());
    assert!(
        !psx.join("images").exists(),
        "per-disc scrape folder emptied"
    );

    // A second look finds only what was left.
    let again = plan_existing(&settings, "psx").unwrap();
    assert!(again.auto.is_empty() && again.duplicates.is_empty());
    // Lemmings: box-front, logo, title-screen; FF7: box-front, mix-image, disc-2/box-front.
    assert_eq!(again.in_place, 6, "found and already good");
    assert_eq!(again.needs_input.len(), 1);
}

#[test]
fn a_scrape_on_one_machine_reaches_every_view_and_the_library_keeps_the_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    library(root);
    let media = canonical_media(root);
    let text = format!(
        "library_root = \"{r}\"\nviews_root = \"{r}/views\"\ndatabase = \"{r}/db.sqlite\"\n\
         undo_dir = \"{r}/undo\"\n\
         [systems]\npsx = {{ folder = \"PSX\" }}\n[uses]\npsx = [\"ra-mednafen_psx\"]\n\
         [nodes.desk]\nplatform = \"es-de\"\nuses = {{ psx = [\"ra-mednafen_psx\"] }}\n\
         [nodes.bato]\nplatform = \"batocera\"\nuses = {{ psx = [\"ra-mednafen_psx\"] }}\n",
        r = root.display()
    );
    let settings: crate::settings::Settings = toml::from_str(&text).unwrap();
    settings.validate().unwrap();
    let mut catalog = Catalog::open(&settings.database).unwrap();
    for node in ["desk", "bato"] {
        crate::ops::apply_additions(&settings, &mut catalog, node).unwrap();
    }
    let desk = settings.view_root("desk");
    let bato = settings.view_root("bato");
    let old_cover = desk.join("downloaded_media/psx/covers/FF7 (USA).png");
    assert_eq!(fs::read_to_string(&old_cover).unwrap(), "c1");

    // ES-DE scrapes: a cover for a game that had none, and a new cover for FF7, written the
    // way scrapers do (delete, then create), so the projection is replaced by a real file.
    write(
        &desk.join("downloaded_media/psx/titlescreens/Lemmings (USA).png"),
        "title",
    );
    fs::remove_file(&old_cover).unwrap();
    write(&old_cover, "new cover");
    write(
        &desk.join("downloaded_media/psx/covers/Not In Library.png"),
        "?",
    );

    // The planner never puts the old link back over the machine's new file.
    let np = crate::ops::plan_node(
        &settings,
        &catalog,
        "desk",
        crate::ops::Target::Settings { allow_empty: false },
    )
    .unwrap();
    assert!(np.plan.replace.is_empty(), "{:?}", np.plan.replace);
    assert!(
        np.plan
            .conflicts
            .iter()
            .any(|c| c.contains("replaced on the machine"))
    );

    let plan = crate::media::plan_ingest(&settings, &catalog, "desk").unwrap();
    assert_eq!(plan.items.len(), 2, "{:?}", plan.items);
    assert_eq!(plan.skipped.len(), 1, "{:?}", plan.skipped);
    let ff7 = plan
        .items
        .iter()
        .find(|i| i.view_path.ends_with("FF7 (USA).png"))
        .unwrap();
    // FF7's art lives under its first disc's name in the library; the set itself is the game.
    assert_eq!(ff7.target, media.join("FF7 (USA)/box-front.png"));
    crate::media::apply_ingest(&settings, &mut catalog, "desk", &plan).unwrap();

    // The library holds real files; the machine's names are now projections of them.
    let title = media.join("Lemmings (USA)/title-screen.png");
    assert_eq!(fs::read_to_string(&title).unwrap(), "title");
    let ino = |p: &Path| fs::metadata(p).unwrap().ino();
    assert_eq!(
        ino(&desk.join("downloaded_media/psx/titlescreens/Lemmings (USA).png")),
        ino(&title)
    );
    assert_eq!(ino(&old_cover), ino(&media.join("FF7 (USA)/box-front.png")));
    assert!(
        crate::media::plan_ingest(&settings, &catalog, "desk")
            .unwrap()
            .items
            .is_empty(),
        "nothing left to import"
    );

    // Batocera picks the new cover up under its own name.
    crate::ops::apply_additions(&settings, &mut catalog, "bato").unwrap();
    assert_eq!(
        fs::read_to_string(bato.join("roms/psx/images/FF7 (USA)-thumb.png")).unwrap(),
        "new cover"
    );
    // ES-DE's own view needs nothing more.
    let np = crate::ops::plan_node(
        &settings,
        &catalog,
        "desk",
        crate::ops::Target::Settings { allow_empty: false },
    )
    .unwrap();
    assert!(
        np.plan.is_noop() && np.plan.conflicts.is_empty(),
        "{:?}",
        np.plan.conflicts
    );
}

#[test]
fn a_bin_without_a_cue_sheet_is_damaged_and_left_out_of_views() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    library(root);
    write(&root.join("PSX/Mystaria (USA).bin"), "no cue");
    let settings: crate::settings::Settings = toml::from_str(&format!(
        "library_root = \"{r}\"\nviews_root = \"{r}/views\"\ndatabase = \"{r}/db\"\n\
         [systems]\npsx = {{ folder = \"PSX\" }}\n[uses]\npsx = [\"ra-mednafen_psx\"]\n\
         [nodes.desk]\nplatform = \"es-de\"\nuses = {{ psx = [\"ra-mednafen_psx\"] }}\n",
        r = root.display()
    ))
    .unwrap();
    let catalog = Catalog::open(&settings.database).unwrap();
    let np = crate::ops::plan_node(
        &settings,
        &catalog,
        "desk",
        crate::ops::Target::Settings { allow_empty: false },
    )
    .unwrap();
    assert!(!dsts(&np.plan.create).iter().any(|d| d.contains("Mystaria")));
    assert!(
        np.warnings
            .iter()
            .any(|w| w.contains("Mystaria (USA).bin: damaged"))
    );
    let summary = crate::ops::summarise_system(&settings, "psx").unwrap();
    assert!(summary.issues.iter().any(|i| i.contains("won't show up")));
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
    let links = project_system(Path::new("psx"), &e.psx, &scan, mode)
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
