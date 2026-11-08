//! Scraped media. The library keeps it frontend-neutral, one folder per game:
//! `media/<system>/<game>/<type>.<ext>` (types in `platforms::MEDIA_TYPES`). Each view gets it
//! in its own frontend's layout, under the names its ROM entries have: ES-DE's
//! `downloaded_media/<system>/covers/<name>.png`, Batocera's `images/<name>-thumb.png` beside
//! the ROMs, and so on (`[layouts.*.media]` in `assets/platforms.toml`).
//!
//! An existing ES-DE `downloaded_media` tree is converted once with `plan_esde_import` /
//! `apply_import`, a library tool: dry run first, undo log, never overwrites.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::catalog::Catalog;
use crate::ownership;
use crate::platforms::{self, Layout, MEDIA_TYPES};
use crate::project::{self, Link, MediaEntry};
use crate::scan::{self, extension, file_name, is_save_name};
use crate::settings::Settings;

/// game folder (relative to the system's media folder) → type → file (relative too).
#[derive(Default)]
pub struct MediaIndex {
    games: HashMap<PathBuf, BTreeMap<String, PathBuf>>,
}

/// Indexes one system's media folder. A system without media is simply empty.
pub fn index_system(dir: &Path) -> Result<MediaIndex> {
    let mut index = MediaIndex::default();
    if !dir.is_dir() {
        return Ok(index);
    }
    let mut files = Vec::new();
    walk(dir, Path::new(""), &mut files)?;
    for file in files {
        let Some(game) = file.parent().filter(|p| !p.as_os_str().is_empty()) else {
            continue;
        };
        let name = file_name(&file);
        let kind = match extension(name) {
            Some(ext) => &name[..name.len() - ext.len() - 1],
            None => name,
        };
        if MEDIA_TYPES.contains(&kind) {
            index
                .games
                .entry(game.to_path_buf())
                .or_default()
                .entry(kind.to_string())
                .or_insert(file);
        }
    }
    Ok(index)
}

fn walk(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    walk_skipping(root, rel, out, &|_| false)
}

/// Like `walk`, but never enters folders `skip` names.
fn walk_skipping(
    root: &Path,
    rel: &Path,
    out: &mut Vec<PathBuf>,
    skip: &dyn Fn(&str) -> bool,
) -> Result<()> {
    let dir = root.join(rel);
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .with_context(|| format!("read {}", dir.display()))?
        .collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let text = name.to_string_lossy();
        if text.starts_with('.') {
            continue;
        }
        let path = rel.join(&name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if !skip(&text) {
                walk_skipping(root, &path, out, skip)?;
            }
        } else if kind.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

/// Folders inside games that hold the game's own data, including its own icons and backdrops
/// (PS3's `PS3_GAME/ICON0.PNG`, `PIC1.PNG`): never scraped media.
const GAME_DATA_DIRS: &[&str] = &["ps3_game", "ps3_update", "ps3_extra", "usrdir"];

/// Folder names that hold scraped media inside a ROM folder, by any frontend or habit.
const MEDIA_DIRS: &[&str] = &[
    "images",
    "videos",
    "manuals",
    "media",
    "covers",
    "boxart",
    "box art",
    "boxarts",
    "screenshots",
    "snaps",
    "snap",
    "titles",
    "wheels",
    "wheel",
    "logos",
    "marquees",
    "fanart",
    "artwork",
    "thumbnails",
];

fn is_game_data_dir(name: &str) -> bool {
    GAME_DATA_DIRS.contains(&name.to_ascii_lowercase().as_str())
}

/// Whether a path inside a ROM folder runs through a folder named for media.
fn in_media_dir(rel: &Path) -> bool {
    rel.parent().is_some_and(|p| {
        p.components().any(|c| {
            MEDIA_DIRS.contains(
                &c.as_os_str()
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .as_str(),
            )
        })
    })
}

/// Links for every entry's media in one frontend's layout. `source_dir` is the system's
/// library media folder, `view_base` where this layout keeps the system's media in the view,
/// `targets` the layout's target pattern → types. An entry for one disc of a set looks in the
/// disc's own folder (`<game>/disc-N/`) before the game's.
pub fn media_links(
    index: &MediaIndex,
    source_dir: &Path,
    view_base: &Path,
    targets: &BTreeMap<String, Vec<String>>,
    entries: &[MediaEntry],
) -> Vec<Link> {
    let mut links = Vec::new();
    for entry in entries {
        let name = entry.view_stem.to_string_lossy();
        for (target, types) in targets {
            let found = entry.lookup().iter().find_map(|folder| {
                let media = index.games.get(folder)?;
                types.iter().find_map(|t| media.get(t))
            });
            let Some(file) = found else {
                continue;
            };
            let mut dst = target.replace("{name}", &name);
            if let Some(ext) = file.extension() {
                dst.push('.');
                dst.push_str(&ext.to_string_lossy());
            }
            links.push(Link {
                dst: view_base.join(dst),
                src: source_dir.join(file),
            });
        }
    }
    links.sort();
    links.dedup_by(|a, b| a.dst == b.dst);
    links
}

/// Extensions treated as media when looking through ROM folders, which also hold the ROMs.
const MEDIA_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "mp4", "mkv", "avi", "webm", "pdf",
];

/// A media file that was in the library before RomServer, and where it seems to belong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Existing {
    /// Relative to the library root.
    pub from: PathBuf,
    /// Best guesses. Both set and no reason: imported without asking.
    pub game: Option<String>,
    /// Set when the file was art for one disc of a set (scraped per disc, before the set had
    /// an `.m3u`): its number, from 1.
    pub disc: Option<usize>,
    pub kind: Option<String>,
    /// Why it needs the user's decision; empty when it doesn't.
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct ExistingPlan {
    /// Files already in the library's layout: found, fine, nothing to do.
    pub in_place: usize,
    /// Imported without asking: to the game's folder, or `disc-N/` when it differs per disc.
    pub auto: Vec<Existing>,
    /// Byte-identical to art the game already has (or is getting): moved to the undo folder.
    pub duplicates: Vec<Existing>,
    pub needs_input: Vec<Existing>,
    /// Every name a game can be given (for the user's choice).
    pub games: Vec<String>,
}

/// What the user chose for a file the import couldn't place on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// "Rename and re-import": this game, this type.
    Import {
        game: String,
        kind: String,
    },
    /// Moved to the undo folder; re-scrape instead.
    Discard,
    Leave,
}

/// Lower-cased name → (the game's media folder, disc number): every single game, every disc
/// set, and each of its discs (old art is often per disc; it belongs to the set).
fn game_names(
    settings: &Settings,
    system: &str,
) -> Result<HashMap<String, (String, Option<usize>)>> {
    let scan = scan::scan_system(&settings.system_root(system)?)?;
    let mut names = HashMap::new();
    let name_of = |path: &Path| project::stem(path).to_string_lossy().into_owned();
    for game in &scan.games {
        match game {
            scan::Game::Single { entry, .. } => {
                let name = name_of(&entry.file);
                names.insert(name.to_ascii_lowercase(), (name, None));
            }
            scan::Game::MultiDisc {
                playlist, discs, ..
            } => {
                let set = name_of(playlist);
                names.insert(set.to_ascii_lowercase(), (set.clone(), None));
                for (i, d) in discs.iter().enumerate() {
                    names.insert(
                        name_of(&d.file).to_ascii_lowercase(),
                        (set.clone(), Some(i + 1)),
                    );
                }
            }
        }
    }
    Ok(names)
}

/// Whether two files hold the same bytes.
fn same_content(a: &Path, b: &Path) -> Result<bool> {
    use std::io::Read as _;
    if std::fs::metadata(a)?.len() != std::fs::metadata(b)?.len() {
        return Ok(false);
    }
    let (mut fa, mut fb) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let n = fa.read(&mut ba)?;
        if n == 0 {
            return Ok(true);
        }
        fb.read_exact(&mut bb[..n])?;
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

/// Reads a file through known layouts' patterns: (game name as written, type).
fn read_pattern<'a>(rel: &Path, patterns: &[(&str, &'a str)]) -> Option<(String, &'a str)> {
    let text = rel.to_string_lossy();
    let bare = match extension(file_name(rel)) {
        Some(e) => &text[..text.len() - e.len() - 1],
        None => &text[..],
    };
    // ASCII lower-casing keeps byte offsets, so the name can be cut from the original.
    let lower = bare.to_ascii_lowercase();
    for (target, kind) in patterns {
        let (before, after) = target.split_once("{name}")?;
        let (before, after) = (before.to_ascii_lowercase(), after.to_ascii_lowercase());
        if lower.len() > before.len() + after.len()
            && lower.starts_with(&before)
            && lower.ends_with(&after)
        {
            return Some((
                bare[before.len()..bare.len() - after.len()].to_string(),
                kind,
            ));
        }
    }
    None
}

/// Finds one system's media from before RomServer: anything in its media folder not yet in the
/// library's own layout, and media files in sub-folders of its ROM folder (Batocera's
/// `images/`, Skyscraper's `media/`, an old `Images/`). Read-only.
pub fn plan_existing(settings: &Settings, system: &str) -> Result<ExistingPlan> {
    let games = game_names(settings, system)?;
    let media_dir = settings.media_dir(system);
    let rom_dir = settings.system_root(system)?;
    // Media kept under another name for the same platform (media/tg16 when the group is
    // pcengine) moves into the group's folder too.
    let group = platforms::media_group(system);
    let alias_dirs: Vec<PathBuf> = platforms::media_group_members(&group)
        .into_iter()
        .filter(|m| *m != group)
        .map(|m| settings.media_root().join(m))
        .filter(|d| d.is_dir())
        .collect();
    let layouts = &platforms::bundled().layouts;
    let pattern_list = |in_tree: bool| -> Vec<(&str, &str)> {
        layouts
            .values()
            .filter(|l| l.media_dir.is_some() == in_tree)
            .flat_map(|l| l.media.iter())
            .filter_map(|(t, kinds)| Some((t.as_str(), kinds.first()?.as_str())))
            .collect()
    };
    let tree_patterns = pattern_list(true);
    let folder_patterns = pattern_list(false);

    let mut found: Vec<(PathBuf, PathBuf, bool)> = Vec::new(); // (base, rel, in ROM folder)
    let mut in_place = 0;
    if media_dir.is_dir() {
        let mut files = Vec::new();
        walk(&media_dir, Path::new(""), &mut files)?;
        for rel in files {
            // Already in the library's layout: <game>[/disc-N]/<type>.<ext>.
            let name = file_name(&rel);
            let kind = extension(name).map_or(name, |e| &name[..name.len() - e.len() - 1]);
            if rel.components().count() >= 2 && MEDIA_TYPES.contains(&kind) {
                in_place += 1;
                continue;
            }
            found.push((media_dir.clone(), rel, false));
        }
    }
    let mut from_alias: HashMap<PathBuf, (String, String)> = HashMap::new();
    for dir in &alias_dirs {
        let mut files = Vec::new();
        walk(dir, Path::new(""), &mut files)?;
        for rel in files {
            // Already per game there: it only changes folder.
            let name = file_name(&rel);
            let kind = extension(name).map_or(name, |e| &name[..name.len() - e.len() - 1]);
            if let (Some(game), true) = (
                rel.parent().filter(|p| p.components().count() == 1),
                MEDIA_TYPES.contains(&kind),
            ) {
                from_alias.insert(
                    dir.join(&rel),
                    (game.to_string_lossy().into_owned(), kind.to_string()),
                );
            }
            found.push((dir.clone(), rel, false));
        }
    }
    // ROM files sit at the top of the ROM folder; media lives in its sub-folders.
    for entry in
        std::fs::read_dir(&rom_dir).with_context(|| format!("read {}", rom_dir.display()))?
    {
        let entry = entry?;
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type()?.is_dir() || dir_name.starts_with('.') || is_game_data_dir(&dir_name)
        {
            continue;
        }
        let mut files = Vec::new();
        walk_skipping(
            &rom_dir,
            Path::new(&entry.file_name()),
            &mut files,
            &is_game_data_dir,
        )?;
        for rel in files {
            let is_media = extension(file_name(&rel))
                .is_some_and(|e| MEDIA_EXTS.contains(&e.to_ascii_lowercase().as_str()));
            if is_media {
                found.push((rom_dir.clone(), rel, true));
            }
        }
    }

    let mut plan = ExistingPlan {
        in_place,
        ..ExistingPlan::default()
    };
    let mut groups: BTreeMap<(String, String), Vec<Existing>> = BTreeMap::new();
    for (base, rel, in_rom_folder) in found {
        let from = base
            .join(&rel)
            .strip_prefix(&settings.library_root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| base.join(&rel));
        let patterns = if in_rom_folder {
            &folder_patterns
        } else {
            &tree_patterns
        };
        let already_per_game = from_alias.get(&base.join(&rel)).cloned();
        let item = match already_per_game {
            Some((game, kind)) => Existing {
                from,
                game: Some(game),
                disc: None,
                kind: Some(kind),
                reason: String::new(),
            },
            None => match read_pattern(&rel, patterns) {
                Some((name, kind)) => match games.get(&name.to_ascii_lowercase()) {
                    Some((game, disc)) => Existing {
                        from,
                        game: Some(game.clone()),
                        disc: *disc,
                        kind: Some(kind.to_string()),
                        reason: String::new(),
                    },
                    None => Existing {
                        from,
                        game: None,
                        disc: None,
                        kind: Some(kind.to_string()),
                        reason: format!("no game in the library is called \u{201c}{name}\u{201d}"),
                    },
                },
                None => {
                    let stem = project::stem(Path::new(file_name(&rel)))
                        .to_string_lossy()
                        .into_owned();
                    match games.get(&stem.to_ascii_lowercase()) {
                        Some((game, disc)) => Existing {
                            from,
                            game: Some(game.clone()),
                            disc: *disc,
                            kind: None,
                            reason: "named after a game, but which kind of media is it?".into(),
                        },
                        None => Existing {
                            from,
                            game: None,
                            disc: None,
                            kind: None,
                            reason: "doesn't match a game or a known media layout".into(),
                        },
                    }
                }
            },
        };
        // Inside a ROM folder, an image is only media if it's named after a game or kept in a
        // folder named for media; anything else there is the games' own data (folder-based
        // games carry icons, backdrops, manuals of their own).
        if in_rom_folder && item.game.is_none() && !in_media_dir(&rel) {
            continue;
        }
        match (&item.game, &item.kind) {
            (Some(game), Some(kind)) if item.reason.is_empty() => groups
                .entry((game.clone(), kind.clone()))
                .or_default()
                .push(item),
            _ => plan.needs_input.push(item),
        }
    }

    // One folder per game. For each game and kind: the game's art is what the library already
    // has, else the set's own, else the lowest disc's. Other discs' copies that are identical
    // are duplicates; ones that differ are that disc's own art (`disc-N/`).
    let library = &settings.library_root;
    for ((game, kind), mut items) in groups {
        items.sort_by_key(|i| i.disc.unwrap_or(0));
        let existing = existing_of_type(&media_dir.join(&game), &kind)
            .into_iter()
            .next();
        let mut base: Option<PathBuf> = existing;
        let mut disc_art: HashMap<usize, PathBuf> = HashMap::new();
        for mut item in items {
            let source = library.join(&item.from);
            let Some(game_art) = &base else {
                // Nothing yet: this one becomes the game's art.
                base = Some(source);
                item.disc = None;
                plan.auto.push(item);
                continue;
            };
            if same_content(&source, game_art)? {
                item.reason = format!("identical to {game}'s {kind}");
                plan.duplicates.push(item);
                continue;
            }
            let Some(n) = item.disc else {
                item.reason = format!("the library already has a different {kind} for {game}");
                plan.needs_input.push(item);
                continue;
            };
            let disc_dir = media_dir.join(&game).join(project::disc_folder(n));
            let disc_existing = existing_of_type(&disc_dir, &kind).into_iter().next();
            match disc_existing.or_else(|| disc_art.get(&n).cloned()) {
                Some(other) if same_content(&source, &other)? => {
                    item.reason = format!("identical to {game}'s disc {n} {kind}");
                    plan.duplicates.push(item);
                }
                Some(_) => {
                    item.reason =
                        format!("the library already has a different disc {n} {kind} for {game}");
                    plan.needs_input.push(item);
                }
                None => {
                    disc_art.insert(n, source);
                    plan.auto.push(item);
                }
            }
        }
    }
    plan.auto.sort_by(|a, b| a.from.cmp(&b.from));
    plan.duplicates.sort_by(|a, b| a.from.cmp(&b.from));
    plan.games = games
        .into_values()
        .map(|(game, _)| game)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    plan.games.sort_by_key(|g| g.to_lowercase());
    Ok(plan)
}

/// `media/<group>/<game>[/disc-N]/<type>.<ext>` for a file, keeping its extension.
fn target(
    settings: &Settings,
    system: &str,
    from: &Path,
    game: &str,
    disc: Option<usize>,
    kind: &str,
) -> PathBuf {
    let mut name = kind.to_string();
    if let Some(ext) = extension(file_name(from)) {
        name.push('.');
        name.push_str(&ext.to_ascii_lowercase());
    }
    let mut dir = settings.media_dir(system).join(game);
    if let Some(n) = disc {
        dir.push(project::disc_folder(n));
    }
    dir.join(name)
}

/// Imports a system's existing media: the automatic matches plus the user's decisions. Everything
/// is validated before anything moves; an import is a rename inside the library (inode kept,
/// never overwrites), a discard moves the file to the undo folder. Returns (imported,
/// discarded, undo log).
pub fn apply_existing(
    settings: &Settings,
    system: &str,
    auto: &[Existing],
    duplicates: &[Existing],
    decided: &[(Existing, Decision)],
) -> Result<(usize, usize, PathBuf)> {
    let library = &settings.library_root;
    let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut discards: Vec<PathBuf> = duplicates.iter().map(|d| d.from.clone()).collect();
    for item in auto {
        let (game, kind) = item
            .game
            .as_ref()
            .zip(item.kind.as_ref())
            .context("incomplete")?;
        moves.push((
            library.join(&item.from),
            target(settings, system, &item.from, game, item.disc, kind),
        ));
    }
    for (item, decision) in decided {
        match decision {
            Decision::Import { game, kind } => {
                if !crate::settings::is_folder_name(game) {
                    bail!("{}: {game:?} isn't a usable game name", item.from.display());
                }
                if !MEDIA_TYPES.contains(&kind.as_str()) {
                    bail!("{}: pick what kind of media it is", item.from.display());
                }
                moves.push((
                    library.join(&item.from),
                    target(settings, system, &item.from, game, None, kind),
                ));
            }
            Decision::Discard => discards.push(item.from.clone()),
            Decision::Leave => {}
        }
    }
    let mut targets = BTreeSet::new();
    for (from, to) in &moves {
        if to.exists() || !targets.insert(to.to_string_lossy().to_lowercase()) {
            bail!(
                "{} would overwrite {}; nothing was changed",
                from.display(),
                to.display()
            );
        }
    }

    ownership::create_dir_all(&settings.undo_dir)
        .with_context(|| format!("create {}", settings.undo_dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let discard_dir = settings.undo_dir.join(format!("discarded-{stamp}"));
    let log = settings
        .undo_dir
        .join(format!("media-existing-{stamp}.tsv"));
    let mut out =
        std::fs::File::create(&log).with_context(|| format!("create {}", log.display()))?;
    ownership::own(&log)?;
    writeln!(out, "# moved\tfrom\tto | discarded\tfrom\tkept at")?;
    for (from, to) in &moves {
        writeln!(out, "moved\t{}\t{}", from.display(), to.display())?;
    }
    for rel in &discards {
        writeln!(
            out,
            "discarded\t{}\t{}",
            library.join(rel).display(),
            discard_dir.join(rel).display()
        )?;
    }
    out.sync_all()?;

    let mut emptied = BTreeSet::new();
    for (from, to) in &moves {
        let dir = to.parent().context("target has no folder")?;
        ownership::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        std::fs::hard_link(from, to)
            .with_context(|| format!("move {} to {}", from.display(), to.display()))?;
        std::fs::remove_file(from).with_context(|| format!("remove {}", from.display()))?;
        emptied.extend(from.parent().map(Path::to_path_buf));
    }
    for rel in &discards {
        let from = library.join(rel);
        move_file(&from, &discard_dir.join(rel))?;
        emptied.extend(from.parent().map(Path::to_path_buf));
    }
    // Remove folders the moves emptied, up to (not including) the system's own folders.
    let stops = [settings.media_dir(system), settings.system_root(system)?];
    for dir in emptied {
        let mut d = Some(dir.as_path());
        while let Some(current) = d {
            if stops.iter().any(|s| s == current)
                || !stops.iter().any(|s| current.starts_with(s))
                || std::fs::remove_dir(current).is_err()
            {
                break;
            }
            d = current.parent();
        }
    }
    Ok((moves.len(), discards.len(), log))
}

/// One file a machine's frontend scraped into its view, and where it goes in the library.
#[derive(Debug, Clone)]
pub struct IngestItem {
    /// Relative to the machine's view root.
    pub view_path: PathBuf,
    /// `media/<system>/<game>/<type>.<ext>`, absolute.
    pub target: PathBuf,
    /// Library files of the same game and type that the new file replaces (newest wins).
    pub replaces: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub struct IngestPlan {
    pub items: Vec<IngestItem>,
    /// Real files in the view that can't be imported, and why.
    pub skipped: Vec<String>,
}

/// Finds the real files (not RomServer's projections) in a machine's media folders and reads
/// each through the machine's layout: its folder pattern gives the type, its name the game.
/// Read-only.
pub fn plan_ingest(settings: &Settings, catalog: &Catalog, node_name: &str) -> Result<IngestPlan> {
    let node = settings.node(node_name)?;
    let platforms = platforms::bundled();
    let layout = platforms.layout(platforms.platform(&node.platform)?);
    let view_root = settings.view_root(node_name);
    let owned = catalog.owned(node_name)?;
    let mut plan = IngestPlan::default();
    if layout.media.is_empty() {
        return Ok(plan);
    }
    let mut taken: BTreeSet<PathBuf> = BTreeSet::new();
    for system in settings.node_systems(node) {
        let base = layout.media_base(system);
        let dir = view_root.join(&base);
        if !dir.is_dir() {
            continue;
        }
        // The names this view lists → the library game each one is.
        let root = settings.system_root(system)?;
        let scan = scan::scan_system(&root)?;
        let projection =
            project::project_system(&layout.rom_dir(system), &root, &scan, node.multi_disc)?;
        let games: HashMap<String, (PathBuf, Option<usize>)> = projection
            .entries
            .iter()
            .map(|e| {
                (
                    e.view_stem.to_string_lossy().into_owned(),
                    (e.game.clone(), e.disc),
                )
            })
            .collect();
        let media_dir = settings.media_dir(system);
        let mut files = Vec::new();
        walk(&dir, Path::new(""), &mut files)?;
        for rel in files {
            let view_path = base.join(&rel);
            let meta = std::fs::symlink_metadata(view_root.join(&view_path))?;
            if owned
                .get(&view_path)
                .is_some_and(|o| crate::sync::still_ours(o, &meta))
            {
                continue; // a projection
            }
            let name = file_name(&rel);
            if is_save_name(name) {
                continue; // the save monitor's business
            }
            match match_target(&rel, layout, &games) {
                Some(((game, disc), kind, ext)) => {
                    let mut file = kind.to_string();
                    if let Some(ext) = &ext {
                        file.push('.');
                        file.push_str(ext);
                    }
                    // A disc's art is the game's while the game has none of that kind;
                    // otherwise it's that disc's own.
                    let mut game_dir = media_dir.join(&game);
                    let game_has_kind = !existing_of_type(&game_dir, kind).is_empty()
                        || taken.iter().any(|t| {
                            t.parent() == Some(game_dir.as_path())
                                && file_name(t).starts_with(&format!("{kind}."))
                        });
                    if let (Some(n), true) = (disc, game_has_kind) {
                        game_dir.push(project::disc_folder(n));
                    }
                    let target = game_dir.join(&file);
                    if !taken.insert(target.clone()) {
                        plan.skipped.push(format!(
                            "{}: another file in this scrape is already the {kind} for {}",
                            view_path.display(),
                            game.display()
                        ));
                        continue;
                    }
                    plan.items.push(IngestItem {
                        view_path,
                        replaces: existing_of_type(&game_dir, kind),
                        target,
                    });
                }
                None => plan.skipped.push(format!(
                    "{}: not a media name this frontend uses for a game in the library",
                    view_path.display()
                )),
            }
        }
    }
    Ok(plan)
}

/// Reads a view file name through the layout's patterns: (game, type, extension).
fn match_target<'a, G: Clone>(
    rel: &Path,
    layout: &'a Layout,
    games: &HashMap<String, G>,
) -> Option<(G, &'a str, Option<String>)> {
    let text = rel.to_string_lossy();
    let name = file_name(rel);
    let ext = extension(name).map(str::to_string);
    let bare = match &ext {
        Some(e) => &text[..text.len() - e.len() - 1],
        None => &text[..],
    };
    for (target, kinds) in &layout.media {
        let (before, after) = target.split_once("{name}")?;
        let Some(middle) = bare
            .strip_prefix(before)
            .and_then(|rest| rest.strip_suffix(after))
        else {
            continue;
        };
        if let (Some(game), Some(kind)) = (games.get(middle), kinds.first()) {
            return Some((game.clone(), kind.as_str(), ext));
        }
    }
    None
}

/// Files in a game's media folder of one type (any extension).
fn existing_of_type(game_dir: &Path, kind: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(game_dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = file_name(p);
            let stem = extension(name).map_or(name, |e| &name[..name.len() - e.len() - 1]);
            stem == kind && p.is_file()
        })
        .collect();
    out.sort();
    out
}

/// Moves a file, copying (and checking the size) when it has to cross filesystems.
fn move_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        ownership::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(18) => {
            let len = std::fs::metadata(from)?.len();
            ownership::copy(from, to).with_context(|| format!("copy to {}", to.display()))?;
            if std::fs::metadata(to)?.len() != len {
                let _ = std::fs::remove_file(to);
                bail!("copy of {} came out the wrong size", from.display());
            }
            std::fs::remove_file(from).with_context(|| format!("remove {}", from.display()))
        }
        Err(e) => Err(e).with_context(|| format!("move {} to {}", from.display(), to.display())),
    }
}

/// Carries out an ingest plan for one machine: the library gets a real copy of each file
/// (written through the library's own path, so it lands on the library's disks), anything it
/// replaces goes to the undo folder, and the view's file becomes a projection of the copy.
/// Returns how many files were imported and the undo log.
pub fn apply_ingest(
    settings: &Settings,
    catalog: &mut Catalog,
    node_name: &str,
    plan: &IngestPlan,
) -> Result<(usize, PathBuf)> {
    let view_root = settings.view_root(node_name);
    let undo_dir = &settings.undo_dir;
    ownership::create_dir_all(undo_dir)
        .with_context(|| format!("create {}", undo_dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let replaced_dir = undo_dir.join(format!("media-replaced-{stamp}"));
    let log = undo_dir.join(format!("media-ingest-{stamp}.tsv"));
    let mut out =
        std::fs::File::create(&log).with_context(|| format!("create {}", log.display()))?;
    ownership::own(&log)?;
    writeln!(
        out,
        "# imported\tview file\tlibrary file | replaced\tlibrary file\tkept at"
    )?;
    let mut recorded = Vec::new();
    for item in &plan.items {
        let view_file = view_root.join(&item.view_path);
        for old in &item.replaces {
            let rel = old.strip_prefix(settings.media_root()).unwrap_or(old);
            let kept = replaced_dir.join(rel);
            move_file(old, &kept)?;
            writeln!(out, "replaced\t{}\t{}", old.display(), kept.display())?;
        }
        let dir = item.target.parent().context("target has no folder")?;
        ownership::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        // Copy beside the target, check it, then link it into place: never overwrites.
        let temporary = dir.join(format!(".romserver-import-{stamp}"));
        let len = std::fs::metadata(&view_file)?.len();
        ownership::copy(&view_file, &temporary)
            .with_context(|| format!("copy {}", view_file.display()))?;
        if std::fs::metadata(&temporary)?.len() != len {
            let _ = std::fs::remove_file(&temporary);
            bail!("copy of {} came out the wrong size", view_file.display());
        }
        std::fs::hard_link(&temporary, &item.target)
            .with_context(|| format!("place {}", item.target.display()))?;
        std::fs::remove_file(&temporary)?;
        // The machine's file becomes a projection of the library's.
        std::fs::remove_file(&view_file)
            .with_context(|| format!("remove {}", view_file.display()))?;
        let id = crate::sync::make_link(&item.target, &view_file)?;
        writeln!(
            out,
            "imported\t{}\t{}",
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
    Ok((plan.items.len(), log))
}
