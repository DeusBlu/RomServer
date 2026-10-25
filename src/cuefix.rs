//! Library tool: checks every cue sheet's `FILE` references and repairs the unambiguous ones.
//!
//! A fix is only proposed when exactly one file in the cue's folder fits, by these rules in
//! order: same name ignoring case; same stem with a different extension; and, for a cue with a
//! single `FILE` line, the one track-like file named after the cue itself. Anything else is
//! reported for a human. Repaired cues are written as a new file and renamed into place
//! (never edited in place), and the original is kept in the undo folder.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::scan::{Class, classify, extension, parse_cue_files, stem};

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Fix(String),
    Unresolved(String),
}

#[derive(Debug)]
pub struct BadRef {
    pub reference: String,
    pub verdict: Verdict,
}

#[derive(Debug)]
pub struct CueReport {
    /// Absolute path of the cue sheet.
    pub cue: PathBuf,
    pub bad: Vec<BadRef>,
}

impl CueReport {
    pub fn fixable(&self) -> bool {
        self.bad
            .iter()
            .all(|b| matches!(b.verdict, Verdict::Fix(_)))
    }
}

pub fn check(root: &Path) -> Result<Vec<CueReport>> {
    let mut out = Vec::new();
    check_dir(&root.to_path_buf(), &mut out)?;
    out.sort_by(|a, b| a.cue.cmp(&b.cue));
    Ok(out)
}

fn check_dir(dir: &PathBuf, out: &mut Vec<CueReport>) -> Result<()> {
    let mut classes: BTreeMap<String, Class> = BTreeMap::new();
    let mut subdirs = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let ft = entry.file_type()?;
        if ft.is_dir() {
            subdirs.push(entry.path());
        } else if ft.is_file() {
            classes.insert(name.clone(), classify(&entry.path(), &name)?);
        }
    }

    let mut refs: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (name, class) in &classes {
        if *class == Class::CueSheet {
            let text = fs::read_to_string(dir.join(name))
                .with_context(|| format!("read {}", dir.join(name).display()))?;
            refs.insert(name, parse_cue_files(&text));
        }
    }
    // Files some cue already uses correctly can't be handed to another cue.
    let used: BTreeSet<&str> = refs
        .values()
        .flatten()
        .filter(|r| classes.contains_key(*r))
        .map(String::as_str)
        .collect();
    let free: Vec<&str> = classes
        .iter()
        .filter(|(n, c)| **c == Class::Data && !used.contains(n.as_str()))
        .map(|(n, _)| n.as_str())
        .collect();

    for (cue, files) in &refs {
        let bad: Vec<BadRef> = files
            .iter()
            .filter(|r| !classes.contains_key(*r))
            .map(|r| BadRef {
                reference: r.clone(),
                verdict: propose(cue, r, files.len(), &free),
            })
            .collect();
        if !bad.is_empty() {
            out.push(CueReport {
                cue: dir.join(cue),
                bad,
            });
        }
    }

    for sub in subdirs {
        check_dir(&sub, out)?;
    }
    Ok(())
}

/// One way a file in the folder can match a broken reference.
type Rule<'a> = Box<dyn Fn(&&str) -> bool + 'a>;

fn propose(cue: &str, reference: &str, file_lines: usize, free: &[&str]) -> Verdict {
    if reference.contains('/') {
        return Verdict::Unresolved("references another folder".into());
    }
    let unique = |cands: Vec<&str>| match cands.as_slice() {
        [one] => Some(Verdict::Fix(one.to_string())),
        [] => None,
        many => Some(Verdict::Unresolved(format!(
            "ambiguous: {}",
            many.join(", ")
        ))),
    };
    let ref_stem = stem(reference);
    let candidates: [Rule; 3] = [
        Box::new(|f| f.eq_ignore_ascii_case(reference)),
        Box::new(|f| stem(f).eq_ignore_ascii_case(ref_stem) && extension(f).is_some()),
        Box::new(|f| {
            file_lines == 1 && stem(f).eq_ignore_ascii_case(stem(cue)) && extension(f).is_some()
        }),
    ];
    for rule in &candidates {
        if let Some(v) = unique(free.iter().copied().filter(|f| rule(f)).collect()) {
            return v;
        }
    }
    Verdict::Unresolved("no matching file in the folder".into())
}

/// Rewrites the fixable cues. Returns the undo log path.
pub fn apply(reports: &[CueReport], undo_root: &Path) -> Result<PathBuf> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let undo_dir = undo_root.join(format!("cuefix-{stamp}"));
    fs::create_dir_all(&undo_dir).with_context(|| format!("create {}", undo_dir.display()))?;
    let log_path = undo_dir.join("undo.tsv");
    let mut log = fs::File::create_new(&log_path)?;

    for (i, report) in reports.iter().filter(|r| r.fixable()).enumerate() {
        let original = fs::read_to_string(&report.cue)
            .with_context(|| format!("read {}", report.cue.display()))?;
        let fixes: BTreeMap<&str, &str> = report
            .bad
            .iter()
            .filter_map(|b| match &b.verdict {
                Verdict::Fix(f) => Some((b.reference.as_str(), f.as_str())),
                Verdict::Unresolved(_) => None,
            })
            .collect();
        let updated = rewrite(&original, &fixes);

        let backup = undo_dir.join(format!("{i:05}.cue"));
        fs::write(&backup, &original)?;
        let tmp = report.cue.with_file_name(format!(
            ".{}.romserver-tmp",
            crate::scan::file_name(&report.cue)
        ));
        fs::write(&tmp, &updated).with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, &report.cue)
            .with_context(|| format!("replace {}", report.cue.display()))?;
        writeln!(
            log,
            "RESTORE\t{}\t{}",
            report.cue.display(),
            backup.display()
        )?;
    }
    Ok(log_path)
}

/// Replaces bad names inside `FILE` lines only, keeping everything else byte for byte.
fn rewrite(text: &str, fixes: &BTreeMap<&str, &str>) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            let refs = parse_cue_files(line);
            match refs
                .first()
                .and_then(|r| fixes.get(r.as_str()).map(|f| (r, f)))
            {
                Some((bad, good)) => line.replacen(bad.as_str(), good, 1),
                None => line.to_string(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib(files: &[(&str, &str)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for (n, body) in files {
            fs::write(tmp.path().join(n), body).unwrap();
        }
        tmp
    }

    fn verdicts(dir: &Path) -> Vec<(String, Verdict)> {
        check(dir)
            .unwrap()
            .into_iter()
            .flat_map(|r| {
                let cue = crate::scan::file_name(&r.cue).to_string();
                r.bad.into_iter().map(move |b| (cue.clone(), b.verdict))
            })
            .collect()
    }

    #[test]
    fn proposes_only_unambiguous_fixes() {
        let tmp = lib(&[
            // Renamed bin: the cue still says (USA).
            (
                "Dark Angel.cue",
                "FILE \"Dark Angel (USA).bin\" BINARY\r\n  TRACK 01 MODE2/2352\r\n",
            ),
            ("Dark Angel.bin", "x"),
            // Wrong extension.
            (
                "Blazing Dragons.cue",
                "FILE \"Blazing Dragons.iso\" BINARY\n",
            ),
            ("Blazing Dragons.bin", "x"),
            // Case only.
            ("Astal.cue", "FILE \"ASTAL.BIN\" BINARY\n"),
            ("Astal.bin", "x"),
            // Healthy cue: its file must not be offered to anyone else.
            ("Good.cue", "FILE \"Good.bin\" BINARY\n"),
            ("Good.bin", "x"),
            // Nothing fits.
            ("Lost.cue", "FILE \"Gone.bin\" BINARY\n"),
        ]);
        let v = verdicts(tmp.path());
        assert_eq!(
            v,
            vec![
                ("Astal.cue".into(), Verdict::Fix("Astal.bin".into())),
                (
                    "Blazing Dragons.cue".into(),
                    Verdict::Fix("Blazing Dragons.bin".into())
                ),
                (
                    "Dark Angel.cue".into(),
                    Verdict::Fix("Dark Angel.bin".into())
                ),
                (
                    "Lost.cue".into(),
                    Verdict::Unresolved("no matching file in the folder".into())
                ),
            ]
        );
    }

    #[test]
    fn apply_rewrites_only_file_lines_as_a_new_file() {
        use std::os::unix::fs::MetadataExt;
        let tmp = lib(&[
            (
                "Dark Angel.cue",
                "FILE \"Dark Angel (USA).bin\" BINARY\r\n  TRACK 01 MODE2/2352\r\n",
            ),
            ("Dark Angel.bin", "x"),
        ]);
        let cue = tmp.path().join("Dark Angel.cue");
        let before = fs::metadata(&cue).unwrap().ino();
        let undo = tempfile::tempdir().unwrap();
        let log = apply(&check(tmp.path()).unwrap(), undo.path()).unwrap();
        assert_eq!(
            fs::read_to_string(&cue).unwrap(),
            "FILE \"Dark Angel.bin\" BINARY\r\n  TRACK 01 MODE2/2352\r\n"
        );
        assert_ne!(
            fs::metadata(&cue).unwrap().ino(),
            before,
            "must be a new file, not an edit"
        );
        assert!(fs::read_to_string(log).unwrap().starts_with("RESTORE\t"));
        assert!(check(tmp.path()).unwrap().is_empty());
    }
}
