//! Turns a system scan into the links a node's view should contain. Pure apart from sniffing
//! extensionless images; never touches the view.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::scan::{Entry, EntryKind, Game, SystemScan, extension, file_name, sniff_image_ext};
use crate::settings::MultiDisc;

/// `dst` is relative to the node's view root; `src` is an absolute library path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Link {
    pub dst: PathBuf,
    pub src: PathBuf,
}

pub struct Projection {
    pub links: Vec<Link>,
    pub warnings: Vec<String>,
    /// One per entry the frontend lists, for naming its scraped media.
    pub entries: Vec<MediaEntry>,
}

/// An entry the frontend lists, and which game's media it shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaEntry {
    /// The entry's path in the view's system folder without its extension: the name its media
    /// must have (ES-DE names media after the file's stem, keeping sub-folders).
    pub view_stem: PathBuf,
    /// The game's media folder (relative to the system's media folder): a single game's stem,
    /// or a disc set's name. One folder per game, disc sets included.
    pub game: PathBuf,
    /// Set when the entry is one disc of a set (shown per disc): its number, from 1.
    pub disc: Option<usize>,
}

/// Folder inside a game's media folder for art that belongs to one disc only.
pub fn disc_folder(disc: usize) -> String {
    format!("disc-{disc}")
}

impl MediaEntry {
    /// Media folders to look in, best first: the disc's own art, then the game's.
    pub fn lookup(&self) -> Vec<PathBuf> {
        match self.disc {
            Some(n) => vec![self.game.join(disc_folder(n)), self.game.clone()],
            None => vec![self.game.clone()],
        }
    }
}

/// A path without its extension (by the scanner's rule for what counts as one).
pub fn stem(path: &Path) -> PathBuf {
    let name = file_name(path);
    match extension(name) {
        Some(ext) => path.with_file_name(&name[..name.len() - ext.len() - 1]),
        None => path.to_path_buf(),
    }
}

/// `view_dir` is the system's folder inside the view (e.g. `roms/psx`).
pub fn project_system(
    view_dir: &Path,
    system_root: &Path,
    scan: &SystemScan,
    mode: MultiDisc,
) -> Result<Projection> {
    let mut p = Projection {
        links: Vec::new(),
        warnings: Vec::new(),
        entries: Vec::new(),
    };
    let mut links = Vec::new();
    let mut link = |src_rel: &Path, dst_rel: PathBuf| {
        links.push(Link {
            dst: view_dir.join(dst_rel),
            src: system_root.join(src_rel),
        });
    };
    let mut entry = |listed: &Path, game: PathBuf, disc: Option<usize>| {
        p.entries.push(MediaEntry {
            view_stem: stem(listed),
            game,
            disc,
        });
    };

    for game in &scan.games {
        match game {
            Game::Single { entry: e, .. } => {
                // A lone extensionless file must regain its extension or no frontend lists it.
                let name = visible_name(system_root, e, &mut p.warnings)?;
                let listed = e.file.with_file_name(name);
                link(&e.file, listed.clone());
                entry(&listed, stem(&e.file), None);
                for t in &e.tracks {
                    link(t, t.clone());
                }
            }
            Game::MultiDisc {
                playlist, discs, ..
            } => {
                let set = stem(playlist);
                match mode {
                    MultiDisc::M3u => {
                        link(playlist, playlist.clone());
                        entry(playlist, set.clone(), None);
                        for d in discs {
                            link(&d.file, d.file.clone());
                            for t in &d.tracks {
                                link(t, t.clone());
                            }
                        }
                    }
                    MultiDisc::Discs => {
                        for (i, d) in discs.iter().enumerate() {
                            let name = visible_name(system_root, d, &mut p.warnings)?;
                            let listed = d.file.with_file_name(name);
                            link(&d.file, listed.clone());
                            entry(&listed, set.clone(), Some(i + 1));
                            for t in &d.tracks {
                                link(t, t.clone());
                            }
                        }
                    }
                    MultiDisc::M3uDirectory => {
                        // Folder named exactly like the playlist, holding the playlist and its
                        // discs. ES-DE lists the folder, so media is named after its stem.
                        let folder = playlist.clone();
                        let inside = |p: &Path| folder.join(file_name(p));
                        link(playlist, inside(playlist));
                        entry(&folder, set.clone(), None);
                        for d in discs {
                            link(&d.file, inside(&d.file));
                            for t in &d.tracks {
                                link(t, inside(t));
                            }
                        }
                    }
                }
            }
        }
    }
    p.links = links;
    Ok(p)
}

/// The file name a frontend needs to see for a launchable entry shown on its own.
fn visible_name(root: &Path, entry: &Entry, warnings: &mut Vec<String>) -> Result<String> {
    let name = file_name(&entry.file);
    if extension(name).is_some() {
        return Ok(name.to_string());
    }
    let ext = match entry.kind {
        EntryKind::CueSheet => Some("cue"),
        EntryKind::File => sniff_image_ext(&root.join(&entry.file))?,
    };
    Ok(match ext {
        Some(ext) => format!("{name}.{ext}"),
        None => {
            warnings.push(format!(
                "{}: no extension and format not recognised",
                entry.file.display()
            ));
            name.to_string()
        }
    })
}
