//! Who owns what RomServer creates.
//!
//! The container runs as root, because Unraid's share settings (`/boot/config/shares`) are
//! readable by root only. Everything it creates, in the shares and in its own config, is handed
//! to `PUID:PGID` (Unraid's `nobody:users` by default) so it stays writable over SMB like the
//! rest of the share. All creation goes through here; with `UMASK=000` folders come out 777 and
//! files 666. Without `PUID`/`PGID` set (a plain local run, the tests) nothing is chowned.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

fn owner() -> Option<(u32, u32)> {
    static OWNER: OnceLock<Option<(u32, u32)>> = OnceLock::new();
    *OWNER.get_or_init(|| {
        let id = |name| std::env::var(name).ok()?.trim().parse::<u32>().ok();
        Some((id("PUID")?, id("PGID")?))
    })
}

/// Hands a path RomServer just created to `PUID:PGID`.
pub fn own(path: &Path) -> io::Result<()> {
    match owner() {
        Some((uid, gid)) => std::os::unix::fs::chown(path, Some(uid), Some(gid)),
        None => Ok(()),
    }
}

/// `std::fs::create_dir_all`, handing every folder it creates (and only those) to `PUID:PGID`.
pub fn create_dir_all(path: &Path) -> io::Result<()> {
    let mut missing: Vec<PathBuf> = Vec::new();
    let mut current = Some(path);
    while let Some(dir) = current {
        if dir.as_os_str().is_empty() || dir.exists() {
            break;
        }
        missing.push(dir.to_path_buf());
        current = dir.parent();
    }
    std::fs::create_dir_all(path)?;
    for dir in missing.iter().rev() {
        own(dir)?;
    }
    Ok(())
}

/// `std::fs::write` for a new file, owned by `PUID:PGID`.
pub fn write(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    std::fs::write(path, contents)?;
    own(path)
}

/// `std::fs::copy` to a new file, owned by `PUID:PGID`.
pub fn copy(from: &Path, to: &Path) -> io::Result<u64> {
    let len = std::fs::copy(from, to)?;
    own(to)?;
    Ok(len)
}

/// Moves a file, creating the target's folders, and copying (checking the size) when it has to
/// cross filesystems.
pub fn move_file(from: &Path, to: &Path) -> anyhow::Result<()> {
    use anyhow::Context;
    if let Some(parent) = to.parent() {
        create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(18) => {
            let len = std::fs::metadata(from)?.len();
            copy(from, to).with_context(|| format!("copy to {}", to.display()))?;
            if std::fs::metadata(to)?.len() != len {
                let _ = std::fs::remove_file(to);
                anyhow::bail!("copy of {} came out the wrong size", from.display());
            }
            std::fs::remove_file(from).with_context(|| format!("remove {}", from.display()))
        }
        Err(e) => Err(e).with_context(|| format!("move {} to {}", from.display(), to.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_only_the_missing_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("a/b/c");
        create_dir_all(&deep).unwrap();
        assert!(deep.is_dir());
        // Existing folders are fine, and a second call changes nothing.
        create_dir_all(&deep).unwrap();
        write(&deep.join("f"), "x").unwrap();
        copy(&deep.join("f"), &deep.join("g")).unwrap();
        assert_eq!(std::fs::read_to_string(deep.join("g")).unwrap(), "x");
    }
}
