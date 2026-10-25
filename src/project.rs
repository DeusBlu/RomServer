//! Turns a system scan into the links a node's view should contain. Pure apart from sniffing
//! extensionless images; never touches the view.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::scan::{Entry, EntryKind, Game, SystemScan, extension, file_name, sniff_image_ext};
use crate::settings::{Frontend, MultiDisc};

/// `dst` is relative to the node's view root; `src` is an absolute library path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Link {
    pub dst: PathBuf,
    pub src: PathBuf,
}

pub struct Projection {
    pub links: Vec<Link>,
    pub warnings: Vec<String>,
}

pub fn project_system(
    frontend: Frontend,
    system: &str,
    system_root: &Path,
    scan: &SystemScan,
    mode: MultiDisc,
) -> Result<Projection> {
    let view_dir = match frontend {
        Frontend::EsDe => PathBuf::from(system),
    };
    let mut p = Projection {
        links: Vec::new(),
        warnings: Vec::new(),
    };
    let mut link = |src_rel: &Path, dst_rel: PathBuf| {
        p.links.push(Link {
            dst: view_dir.join(dst_rel),
            src: system_root.join(src_rel),
        });
    };

    for game in &scan.games {
        match game {
            Game::Single { entry, .. } => {
                // A lone extensionless file must regain its extension or no frontend lists it.
                let name = visible_name(system_root, entry, &mut p.warnings)?;
                link(&entry.file, entry.file.with_file_name(name));
                for t in &entry.tracks {
                    link(t, t.clone());
                }
            }
            Game::MultiDisc {
                playlist, discs, ..
            } => match mode {
                MultiDisc::M3u => {
                    link(playlist, playlist.clone());
                    for d in discs {
                        link(&d.file, d.file.clone());
                        for t in &d.tracks {
                            link(t, t.clone());
                        }
                    }
                }
                MultiDisc::Discs => {
                    for d in discs {
                        let name = visible_name(system_root, d, &mut p.warnings)?;
                        link(&d.file, d.file.with_file_name(name));
                        for t in &d.tracks {
                            link(t, t.clone());
                        }
                    }
                }
                MultiDisc::M3uDirectory => {
                    // Folder named exactly like the playlist, holding the playlist and its discs.
                    let folder = playlist.clone();
                    let inside = |p: &Path| folder.join(file_name(p));
                    link(playlist, inside(playlist));
                    for d in discs {
                        link(&d.file, inside(&d.file));
                        for t in &d.tracks {
                            link(t, inside(t));
                        }
                    }
                }
            },
        }
    }
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
