//! Walks one system folder of the library and groups its files into games.
//!
//! A multi-disc set is an `.m3u` plus the disc files it lists; a disc is either a cue sheet
//! (with or without the `.cue` extension) plus the tracks its `FILE` lines name, or a single
//! image. Every file a set references must live in the same folder as the `.m3u`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const SAVE_EXTS: &[&str] = &[
    "srm", "sav", "mcr", "mcd", "state", "dsv", "eep", "fla", "sra",
];
const IGNORED_EXTS: &[&str] = &[
    // Not "md": that is the Mega Drive ROM extension.
    "txt", "nfo", "jpg", "jpeg", "png", "gif", "bmp", "webp", "xml", "pdf", "db", "ini", "cfg",
    "log", "url", "lnk", "dat", "sfv", "md5", "sha1",
];
const CUE_KEYWORDS: &[&str] = &[
    "FILE",
    "REM",
    "CATALOG",
    "PERFORMER",
    "TITLE",
    "CDTEXTFILE",
    "SONGWRITER",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A cue sheet; its tracks are the files named by its `FILE` lines.
    CueSheet,
    /// A self-contained file: a cartridge ROM, `.iso`, `.chd`, …
    File,
}

/// One launchable file plus the files it pulls in. Paths are relative to the system folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub file: PathBuf,
    pub kind: EntryKind,
    pub tracks: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Game {
    MultiDisc {
        name: String,
        playlist: PathBuf,
        discs: Vec<Entry>,
    },
    Single {
        name: String,
        entry: Entry,
    },
}

impl Game {
    pub fn name(&self) -> &str {
        match self {
            Game::MultiDisc { name, .. } | Game::Single { name, .. } => name,
        }
    }
}

#[derive(Debug, Default)]
pub struct SystemScan {
    pub games: Vec<Game>,
    pub saves: Vec<PathBuf>,
    pub ignored: Vec<PathBuf>,
    pub issues: Vec<String>,
}

/// File extension, but only when it looks like one. Names such as `Driver 2 (USA) (v1.1)` or
/// extensionless cue sheets contain dots that aren't extensions.
pub fn extension(name: &str) -> Option<&str> {
    let (_, ext) = name.rsplit_once('.')?;
    (!ext.is_empty() && ext.len() <= 8 && ext.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then_some(ext)
}

pub fn stem(name: &str) -> &str {
    match extension(name) {
        Some(ext) => &name[..name.len() - ext.len() - 1],
        None => name,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    Playlist,
    CueSheet,
    Save,
    Ignored,
    Data,
}

pub(crate) fn classify(path: &Path, name: &str) -> Result<Class> {
    let Some(ext) = extension(name) else {
        return Ok(if looks_like_cue(path)? {
            Class::CueSheet
        } else {
            Class::Data
        });
    };
    let ext = ext.to_ascii_lowercase();
    Ok(match ext.as_str() {
        "m3u" => Class::Playlist,
        "cue" => Class::CueSheet,
        e if SAVE_EXTS.contains(&e) => Class::Save,
        // RetroArch numbered states: .state1, .state2, …
        e if e.starts_with("state") && e[5..].bytes().all(|b| b.is_ascii_digit()) => Class::Save,
        e if IGNORED_EXTS.contains(&e) => Class::Ignored,
        _ => Class::Data,
    })
}

fn read_head(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    fs::File::open(path)
        .with_context(|| format!("open {}", path.display()))?
        .take(limit)
        .read_to_end(&mut buf)?;
    Ok(buf)
}

fn looks_like_cue(path: &Path) -> Result<bool> {
    let head = read_head(path, 4096)?;
    if head.contains(&0) {
        return Ok(false);
    }
    let text = String::from_utf8_lossy(&head);
    let mut has_file = false;
    for line in text.trim_start_matches('\u{feff}').lines() {
        let Some(word) = line.split_whitespace().next() else {
            continue;
        };
        let word = word.to_ascii_uppercase();
        if !CUE_KEYWORDS.contains(&word.as_str()) && word != "TRACK" && word != "INDEX" {
            return Ok(false);
        }
        has_file |= word == "FILE";
    }
    Ok(has_file)
}

/// Guesses the extension of an extensionless disc image from its contents.
pub fn sniff_image_ext(path: &Path) -> Result<Option<&'static str>> {
    let head = read_head(path, 0x8006)?;
    if head.starts_with(b"MComprHD") {
        return Ok(Some("chd"));
    }
    if head.get(0x8001..0x8006) == Some(b"CD001") {
        return Ok(Some("iso"));
    }
    Ok(None)
}

fn normalise_ref(r: &str) -> String {
    r.trim().replace('\\', "/")
}

pub fn parse_cue_files(text: &str) -> Vec<String> {
    text.trim_start_matches('\u{feff}')
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line
                .get(..4)?
                .eq_ignore_ascii_case("FILE")
                .then(|| line[4..].trim())?;
            let name = if let Some(quoted) = rest.strip_prefix('"') {
                quoted.split('"').next()?
            } else {
                rest.rsplit_once(char::is_whitespace)
                    .map_or(rest, |(n, _)| n)
            };
            Some(normalise_ref(name))
        })
        .collect()
}

pub fn parse_m3u(text: &str) -> Vec<String> {
    text.trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(normalise_ref)
        .collect()
}

pub fn scan_system(root: &Path) -> Result<SystemScan> {
    let mut out = SystemScan::default();
    scan_dir(root, Path::new(""), &mut out)?;
    out.games.sort_by(|a, b| a.name().cmp(b.name()));
    Ok(out)
}

fn scan_dir(root: &Path, rel: &Path, out: &mut SystemScan) -> Result<()> {
    let dir = root.join(rel);
    let mut classes: BTreeMap<String, Class> = BTreeMap::new();
    let mut subdirs = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            out.issues
                .push(format!("{}: non-UTF-8 file name skipped", dir.display()));
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let ft = entry.file_type()?;
        if ft.is_dir() {
            subdirs.push(name);
        } else if ft.is_file() {
            classes.insert(name.clone(), classify(&entry.path(), &name)?);
        }
    }

    let rel_path = |name: &str| rel.join(name);

    let mut cue_tracks: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (name, class) in &classes {
        if *class == Class::CueSheet {
            let text = fs::read_to_string(dir.join(name))
                .with_context(|| format!("read cue {}", dir.join(name).display()))?;
            cue_tracks.insert(name, parse_cue_files(&text));
        }
    }
    // Tracks are never games of their own, even if their cue sheet turns out broken.
    let all_tracks: BTreeSet<&str> = cue_tracks.values().flatten().map(String::as_str).collect();

    // Checks that a disc file and its tracks exist in this folder.
    let resolve = |disc: &str| -> std::result::Result<Entry, String> {
        if disc.contains('/') {
            return Err(format!("{disc}: not in the same folder"));
        }
        match classes.get(disc) {
            None => Err(format!("{disc}: missing")),
            Some(Class::CueSheet) => {
                let tracks = &cue_tracks[disc];
                if let Some(t) = tracks
                    .iter()
                    .find(|t| t.contains('/') || !classes.contains_key(*t))
                {
                    return Err(format!(
                        "{disc}: track {t} missing or not in the same folder"
                    ));
                }
                Ok(Entry {
                    file: rel_path(disc),
                    kind: EntryKind::CueSheet,
                    tracks: tracks.iter().map(|t| rel_path(t)).collect(),
                })
            }
            Some(Class::Data) => Ok(Entry {
                file: rel_path(disc),
                kind: EntryKind::File,
                tracks: vec![],
            }),
            Some(other) => Err(format!("{disc}: not a disc ({other:?})")),
        }
    };

    let mut claimed: BTreeSet<&str> = BTreeSet::new();
    for (name, class) in &classes {
        if *class != Class::Playlist {
            continue;
        }
        let path = dir.join(name);
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let discs: std::result::Result<Vec<Entry>, String> =
            parse_m3u(&text).iter().map(|d| resolve(d)).collect();
        match discs {
            Ok(discs) if !discs.is_empty() => {
                claimed.insert(name);
                for d in &discs {
                    claimed.insert(classes.get_key_value(file_name(&d.file)).unwrap().0);
                }
                out.games.push(Game::MultiDisc {
                    name: stem(name).to_string(),
                    playlist: rel_path(name),
                    discs,
                });
            }
            Ok(_) => out
                .issues
                .push(format!("{}: empty playlist", path.display())),
            Err(e) => out.issues.push(format!("{}: {e}", path.display())),
        }
    }

    for (name, class) in &classes {
        let name = name.as_str();
        match class {
            Class::Save => out.saves.push(rel_path(name)),
            Class::Ignored => out.ignored.push(rel_path(name)),
            Class::Playlist => {}
            _ if claimed.contains(name) || all_tracks.contains(name) => {}
            Class::CueSheet | Class::Data => match resolve(name) {
                Ok(entry) => out.games.push(Game::Single {
                    name: stem(name).to_string(),
                    entry,
                }),
                Err(e) => out.issues.push(format!("{}: {e}", dir.display())),
            },
        }
    }

    for sub in subdirs {
        scan_dir(root, &rel.join(sub), out)?;
    }
    Ok(())
}

pub fn file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_ignores_dots_that_are_not_extensions() {
        assert_eq!(extension("Game (USA).bin"), Some("bin"));
        assert_eq!(extension("Driver 2 (USA) (Disc 1) (v1.1)"), None);
        assert_eq!(extension("Chrono Cross (USA) (Disc 1)"), None);
        assert_eq!(extension("Dr. Mario (USA).nes"), Some("nes"));
        assert_eq!(stem("Driver 2 (USA) (v1.1).m3u"), "Driver 2 (USA) (v1.1)");
    }

    #[test]
    fn cue_file_lines() {
        let cue = "\u{feff}FILE \"A (Track 1).bin\" BINARY\n  TRACK 01 MODE2/2352\nFILE b.bin BINARY\nfile \"dir\\c.bin\" WAVE\n";
        assert_eq!(
            parse_cue_files(cue),
            vec!["A (Track 1).bin", "b.bin", "dir/c.bin"]
        );
    }
}
