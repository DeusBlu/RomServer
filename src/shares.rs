//! What the server exports: Unraid's share settings (when mounted into the container) plus
//! hand-set shares, and the mapping from a server path to the remote a client mounts.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::settings::{Protocol, Settings, Share};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Unraid,
    Manual,
}

#[derive(Debug, Clone)]
pub struct ShareInfo {
    pub share: Share,
    pub source: Source,
}

/// Whether Unraid's share settings could be read, for the Settings page.
pub enum UnraidStatus {
    NotConfigured,
    NotMounted(PathBuf),
    /// Mounted, but RomServer can't read it (it's root-only on Unraid).
    Unreadable {
        path: PathBuf,
        error: String,
    },
    Read {
        path: PathBuf,
        shares: usize,
    },
}

/// Parses one `/boot/config/shares/<name>.cfg`: `key="value"` lines.
pub fn parse_unraid_cfg(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect()
}

/// `e` = exported, `eh` = exported but hidden, `-` = not exported.
fn exported(value: Option<&String>) -> bool {
    value.is_some_and(|v| v.starts_with('e'))
}

/// Reads every share config in `config_dir`. Configs whose folder no longer exists under
/// `user_root` are skipped: Unraid leaves them behind when a share is deleted by hand.
pub fn read_unraid(config_dir: &Path, user_root: &Path) -> Result<BTreeMap<String, Share>> {
    let mut out = BTreeMap::new();
    for entry in
        std::fs::read_dir(config_dir).with_context(|| format!("read {}", config_dir.display()))?
    {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "cfg") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let share_path = user_root.join(name);
        if !share_path.is_dir() {
            continue;
        }
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let cfg = parse_unraid_cfg(&text);
        out.insert(
            name.to_string(),
            Share {
                path: share_path,
                smb: exported(cfg.get("shareExport")),
                nfs: exported(cfg.get("shareExportNFS")),
            },
        );
    }
    Ok(out)
}

/// Unraid's shares (if readable) with the hand-set ones layered on top by name.
pub fn effective(settings: &Settings) -> (BTreeMap<String, ShareInfo>, UnraidStatus) {
    let mut out = BTreeMap::new();
    let status = match &settings.network.unraid_share_config {
        None => UnraidStatus::NotConfigured,
        Some(dir) if !dir.is_dir() => UnraidStatus::NotMounted(dir.clone()),
        Some(dir) => match read_unraid(dir, &settings.network.unraid_user_root) {
            Ok(found) => {
                let count = found.len();
                for (name, share) in found {
                    out.insert(
                        name,
                        ShareInfo {
                            share,
                            source: Source::Unraid,
                        },
                    );
                }
                UnraidStatus::Read {
                    path: dir.clone(),
                    shares: count,
                }
            }
            Err(e) => UnraidStatus::Unreadable {
                path: dir.clone(),
                error: format!("{e:#}"),
            },
        },
    };
    for (name, share) in &settings.shares {
        out.insert(
            name.clone(),
            ShareInfo {
                share: share.clone(),
                source: Source::Manual,
            },
        );
    }
    (out, status)
}

/// What to do on the server so `path` can be mounted, for when `remote_for` fails. Written
/// for someone who doesn't yet know which folders need sharing.
pub fn sharing_advice(
    settings: &Settings,
    shares: &BTreeMap<String, ShareInfo>,
    path: &Path,
) -> Option<String> {
    let protocol = match settings.network.protocol {
        Protocol::Smb => "SMB",
        Protocol::Nfs => "NFS",
    };
    if settings.network.server.is_empty() {
        return Some("Set the server address clients use under Network.".into());
    }
    let user_root = &settings.network.unraid_user_root;
    let top = path.strip_prefix(user_root).ok()?.components().next()?;
    let name = top.as_os_str().to_string_lossy();
    let config = settings.network.unraid_share_config.as_ref();
    let unraid_readable = config.is_some_and(|d| std::fs::read_dir(d).is_ok());
    // Mounted but unreadable: every share looks missing, so say that instead of blaming them.
    if let Some(dir) = config.filter(|d| d.is_dir() && !unraid_readable) {
        return Some(format!(
            "RomServer can't read Unraid's share settings at {} (they're readable by root only), \
             so it can't tell which shares are exported. Run the container as root (its default), \
             or add the shares by hand under Settings → Shares.",
            dir.display()
        ));
    }
    if shares.contains_key(name.as_ref()) {
        return Some(format!(
            "The \"{name}\" share needs sharing over {protocol}: in Unraid, open Shares → {name} \
             and set {protocol} Security Settings → Export to Yes."
        ));
    }
    if !user_root.join(top).is_dir() {
        return Some(format!(
            "{} doesn't exist yet; applying a machine's view creates it. Then share \"{name}\" over \
             {protocol}: in Unraid, open Shares → {name} and set {protocol} Security Settings → \
             Export to Yes.",
            user_root.join(top).display()
        ));
    }
    Some(if unraid_readable {
        format!(
            "\"{name}\" has no Unraid share settings yet. In Unraid, open Shares → {name}, set \
             {protocol} Security Settings → Export to Yes and apply."
        )
    } else {
        format!(
            "Share {} over {protocol} on the server, or add it by hand under Shares.",
            user_root.join(top).display()
        )
    })
}

/// What a client puts in a mount unit's `What=` and `Type=`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub what: String,
    pub fs_type: &'static str,
}

/// The remote for a server path: the deepest share containing it, exported over `protocol`.
pub fn remote_for(
    path: &Path,
    shares: &BTreeMap<String, ShareInfo>,
    server: &str,
    protocol: Protocol,
) -> Result<Remote> {
    if server.is_empty() {
        bail!("the server address isn't set");
    }
    let (name, info) = shares
        .iter()
        .filter(|(_, i)| path.starts_with(&i.share.path))
        .max_by_key(|(_, i)| i.share.path.components().count())
        .with_context(|| format!("no share contains {}", path.display()))?;
    let rest = path
        .strip_prefix(&info.share.path)
        .expect("filtered on starts_with");
    match protocol {
        Protocol::Smb => {
            if !info.share.smb {
                bail!("share {name} isn't exported over SMB");
            }
            let mut what = format!("//{server}/{name}");
            for part in rest.components() {
                what.push('/');
                what.push_str(&part.as_os_str().to_string_lossy());
            }
            Ok(Remote {
                what,
                fs_type: "cifs",
            })
        }
        Protocol::Nfs => {
            if !info.share.nfs {
                bail!("share {name} isn't exported over NFS");
            }
            Ok(Remote {
                what: format!("{server}:{}", path.display()),
                fs_type: "nfs",
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROMS_CFG: &str = r#"# Generated settings:
shareComment=""
shareUseCache="no"
shareExport="e"
shareSecurity="public"
shareExportNFS="-"
shareSecurityNFS="public"
"#;

    #[test]
    fn reads_unraid_configs_and_skips_deleted_shares() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("shares");
        let user = tmp.path().join("user");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(user.join("ROMS")).unwrap();
        std::fs::create_dir_all(user.join("views")).unwrap();
        std::fs::write(config.join("ROMS.cfg"), ROMS_CFG).unwrap();
        std::fs::write(
            config.join("views.cfg"),
            "shareExport=\"eh\"\nshareExportNFS=\"e\"\n",
        )
        .unwrap();
        std::fs::write(config.join("rs-linktest.cfg"), "shareExport=\"e\"\n").unwrap();

        let found = read_unraid(&config, &user).unwrap();
        assert_eq!(found.keys().collect::<Vec<_>>(), ["ROMS", "views"]);
        assert!(found["ROMS"].smb && !found["ROMS"].nfs);
        assert!(
            found["views"].smb && found["views"].nfs,
            "hidden still counts"
        );
        assert_eq!(found["ROMS"].path, user.join("ROMS"));
    }

    fn shares() -> BTreeMap<String, ShareInfo> {
        let manual = |path: &str, smb, nfs| ShareInfo {
            share: Share {
                path: PathBuf::from(path),
                smb,
                nfs,
            },
            source: Source::Manual,
        };
        BTreeMap::from([
            ("ROMS".to_string(), manual("/mnt/user/ROMS", true, false)),
            (
                "saves".to_string(),
                manual("/mnt/user/ROMS/saves", true, true),
            ),
        ])
    }

    #[test]
    fn advice_says_what_to_share_in_unraid() {
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path().join("user");
        std::fs::create_dir_all(user.join("views")).unwrap();
        let mut settings: Settings = toml::from_str(&format!(
            "library_root = \"{u}/ROMS\"\nviews_root = \"{u}/views\"\ndatabase = \"/tmp/x\"\n\
             [network]\nserver = \"nas\"\nunraid_user_root = \"{u}\"\nunraid_share_config = \"{u}\"\n",
            u = user.display()
        ))
        .unwrap();
        let mut s = shares();
        let advice = |s: &BTreeMap<String, ShareInfo>, settings: &Settings, p: &str| {
            sharing_advice(settings, s, &user.join(p)).unwrap()
        };
        // An auto-created share has a folder but no settings file yet.
        assert!(advice(&s, &settings, "views/deck").contains("has no Unraid share settings yet"));
        // Not created yet: the first apply makes it.
        assert!(advice(&s, &settings, "other/x").contains("doesn't exist yet"));
        s.insert(
            "views".into(),
            ShareInfo {
                share: Share {
                    path: user.join("views"),
                    smb: false,
                    nfs: false,
                },
                source: Source::Unraid,
            },
        );
        assert!(advice(&s, &settings, "views/deck").contains("Shares → views"));
        settings.network.server.clear();
        assert!(advice(&s, &settings, "views/deck").contains("server address"));
        assert!(sharing_advice(&settings, &s, Path::new("/elsewhere")).is_some());
    }

    #[test]
    fn remote_uses_the_deepest_share_and_its_protocol() {
        let s = shares();
        let r = remote_for(
            Path::new("/mnt/user/ROMS/PSX"),
            &s,
            "192.168.1.1",
            Protocol::Smb,
        )
        .unwrap();
        assert_eq!(r.what, "//192.168.1.1/ROMS/PSX");
        assert_eq!(r.fs_type, "cifs");
        let r = remote_for(
            Path::new("/mnt/user/ROMS/saves/snes-saves"),
            &s,
            "nas",
            Protocol::Smb,
        )
        .unwrap();
        assert_eq!(r.what, "//nas/saves/snes-saves");
        let r = remote_for(
            Path::new("/mnt/user/ROMS/saves/snes-saves"),
            &s,
            "nas",
            Protocol::Nfs,
        )
        .unwrap();
        assert_eq!(r.what, "nas:/mnt/user/ROMS/saves/snes-saves");
        assert!(remote_for(Path::new("/mnt/user/ROMS/PSX"), &s, "nas", Protocol::Nfs).is_err());
        assert!(remote_for(Path::new("/mnt/user/views/x"), &s, "nas", Protocol::Smb).is_err());
        assert!(remote_for(Path::new("/mnt/user/ROMS/PSX"), &s, "", Protocol::Smb).is_err());
    }
}
