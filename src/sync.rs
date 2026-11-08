//! Diffs the links a view should contain against what's on disk, then applies the difference.
//!
//! Safety rules:
//! - Only paths recorded as owned are ever removed or replaced.
//! - A view file that is the last link to its data (`nlink == 1`) and is no longer wanted is
//!   never removed: the library file is gone, so deleting it would destroy the only copy.
//! - View files are never written to. A change is always remove + fresh link; views only
//!   change by adding and removing links.
//! - Anything at a desired path that RomServer doesn't own is reported, never touched.
//! - An owned path is only relinked or removed while it still holds the file RomServer linked
//!   there (same inode as recorded). A different file at that path was written on the machine
//!   (a re-scrape, say): it is reported for "New media added", never overwritten.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::catalog::{self, Catalog, Owned};
use crate::project::Link;

#[derive(Debug, Default)]
pub struct Plan {
    pub create: Vec<Link>,
    /// Owned, but now points at different data (e.g. a regenerated `.m3u`).
    pub replace: Vec<Link>,
    /// Owned and no longer wanted.
    pub remove: Vec<PathBuf>,
    /// Already correct; recorded as owned (adopts links left by an interrupted apply).
    pub keep: Vec<Link>,
    /// Desired paths occupied by something RomServer doesn't own, or unsafe to change.
    pub conflicts: Vec<String>,
}

impl Plan {
    pub fn is_noop(&self) -> bool {
        self.create.is_empty() && self.replace.is_empty() && self.remove.is_empty()
    }
}

pub fn identity(m: &fs::Metadata) -> (u64, u64) {
    (m.dev(), m.ino())
}

fn stat(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
    }
}

/// Whether the file at an owned path is still RomServer's link. Links recorded before
/// identities were kept count as RomServer's while they're still linked from elsewhere.
pub fn still_ours(owned: &Owned, at_path: &fs::Metadata) -> bool {
    match owned.identity {
        Some(id) => id == identity(at_path),
        None => at_path.nlink() > 1,
    }
}

pub fn plan(view_root: &Path, links: Vec<Link>, owned: &BTreeMap<PathBuf, Owned>) -> Result<Plan> {
    // Clients reach views over SMB, which ignores case, so names may only differ in more than case.
    let mut seen: BTreeMap<String, &Link> = BTreeMap::new();
    for l in &links {
        let key = l.dst.to_string_lossy().to_lowercase();
        if let Some(prev) = seen.insert(key, l) {
            bail!(
                "two files project to the same view path {} (case-insensitive): {} and {}",
                l.dst.display(),
                prev.src.display(),
                l.src.display()
            );
        }
    }

    let mut plan = Plan::default();
    let desired: BTreeSet<&Path> = links.iter().map(|l| l.dst.as_path()).collect();

    for link in &links {
        let src = stat(&link.src)?
            .with_context(|| format!("library file vanished: {}", link.src.display()))?;
        match stat(&view_root.join(&link.dst))? {
            None => plan.create.push(link.clone()),
            Some(dst) if identity(&dst) == identity(&src) => plan.keep.push(link.clone()),
            Some(dst) => match owned.get(&link.dst) {
                None => plan.conflicts.push(format!(
                    "{}: exists and isn't RomServer's; left alone",
                    link.dst.display()
                )),
                // RomServer's own link, and the library has a newer file for this path (a
                // regenerated .m3u, re-scraped art): relinking loses nothing the library wants.
                Some(o) if still_ours(o, &dst) => plan.replace.push(link.clone()),
                Some(_) => plan.conflicts.push(format!(
                    "{}: replaced on the machine (new media? use New media added); left alone",
                    link.dst.display()
                )),
            },
        }
    }

    for (path, o) in owned {
        if desired.contains(path.as_path()) {
            continue;
        }
        match stat(&view_root.join(path))? {
            None => plan.remove.push(path.clone()), // already gone; just forget it
            Some(m) if !still_ours(o, &m) => plan.conflicts.push(format!(
                "{}: replaced on the machine; not removed",
                path.display()
            )),
            Some(m) if m.nlink() <= 1 => plan.conflicts.push(format!(
                "{}: last copy of its data; not removed",
                path.display()
            )),
            Some(_) => plan.remove.push(path.clone()),
        }
    }
    Ok(plan)
}

pub fn apply(catalog: &mut Catalog, node: &str, view_root: &Path, plan: &Plan) -> Result<()> {
    let tx = catalog.transaction()?;

    for path in &plan.remove {
        let abs = view_root.join(path);
        match fs::remove_file(&abs) {
            Ok(()) => prune_empty_dirs(view_root, &abs),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("remove {}", abs.display())),
        }
        catalog::forget(&tx, node, path)?;
    }
    for link in &plan.replace {
        let abs = view_root.join(&link.dst);
        fs::remove_file(&abs).with_context(|| format!("remove {}", abs.display()))?;
        let id = make_link(&link.src, &abs)?;
        catalog::record(&tx, node, &link.dst, &link.src, id)?;
    }
    for link in &plan.create {
        let id = make_link(&link.src, &view_root.join(&link.dst))?;
        catalog::record(&tx, node, &link.dst, &link.src, id)?;
    }
    for link in &plan.keep {
        let id = identity(
            &fs::metadata(&link.src).with_context(|| format!("stat {}", link.src.display()))?,
        );
        catalog::record(&tx, node, &link.dst, &link.src, id)?;
    }
    tx.commit()?;
    Ok(())
}

/// Links `dst` to `src` and returns the identity it now shares.
pub fn make_link(src: &Path, dst: &Path) -> Result<(u64, u64)> {
    if let Some(parent) = dst.parent() {
        crate::ownership::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    fs::hard_link(src, dst).map_err(|e| {
        let hint = if e.raw_os_error() == Some(18) {
            " (EXDEV: library and views must be reached through the same mount; see PLAN.md)"
        } else {
            ""
        };
        anyhow::anyhow!("link {} -> {}: {e}{hint}", src.display(), dst.display())
    })?;
    Ok(identity(&fs::metadata(dst)?))
}

/// Removes now-empty directories up to (not including) the view root. `remove_dir` refuses
/// non-empty directories, so nothing else can be lost.
fn prune_empty_dirs(view_root: &Path, removed: &Path) {
    let mut dir = removed.parent();
    while let Some(d) = dir {
        if d == view_root || !d.starts_with(view_root) || fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}
