//! Client mount bundles: what each machine mounts (its view, its emulators' save folders) and
//! the files that set it up: systemd `.mount` + `.automount` units with an install script, or
//! Batocera's boot config and `custom.sh`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::emulators::{EmulatorInfo, Kind};
use crate::platforms::{self, Mounter, RetroArchSort, VIEW_ROMS};
use crate::settings::{Node, Protocol, Settings};
use crate::shares::{self, Remote, ShareInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountKind {
    Roms,
    Media,
    Saves,
    States,
}

impl MountKind {
    /// Whether existing local files are copied into the share when the mount is installed.
    fn imports_local_files(self) -> bool {
        matches!(self, MountKind::Saves | MountKind::States)
    }
}

#[derive(Debug, Clone)]
pub struct Mount {
    pub kind: MountKind,
    pub label: String,
    /// Path on the client machine.
    pub local: PathBuf,
    /// Path on the server.
    pub server: PathBuf,
    pub remote: Remote,
}

pub struct MountPlan {
    pub node: String,
    pub platform_label: String,
    pub mounter: Mounter,
    pub units_dir: String,
    pub protocol: Protocol,
    pub options: String,
    pub uid: u32,
    pub gid: u32,
    pub mounts: Vec<Mount>,
    pub warnings: Vec<String>,
    pub notes: Vec<String>,
}

/// One mount before its remote is resolved.
struct Wanted {
    kind: MountKind,
    label: String,
    local: PathBuf,
    server: PathBuf,
}

/// Works out every mount for a machine. Problems that only affect one mount (an unknown save
/// path, a share that isn't exported) become warnings; the rest of the plan still stands.
pub fn plan_mounts(
    settings: &Settings,
    node_name: &str,
    emulators: &HashMap<&str, &EmulatorInfo>,
) -> Result<MountPlan> {
    let node = settings.node(node_name)?;
    let platforms = platforms::bundled();
    let platform = platforms.platform(&node.platform)?;
    let layout = platforms.layout(platform);
    let home = node.home.as_deref().unwrap_or(&platform.default_home);
    let base = node.base.as_deref().unwrap_or(&platform.base);
    let local = |template: &str| platforms::expand(template, home, base);
    let view = settings.view_root(node_name);
    let mut warnings = Vec::new();
    let mut wanted = vec![Wanted {
        kind: MountKind::Roms,
        label: "ROMs".into(),
        local: local(&platform.roms),
        server: view.join(VIEW_ROMS),
    }];
    // Frontends that keep media in a tree of their own get it as a separate mount; the others
    // find it inside their ROM folders.
    if let (Some(media), Some(media_dir)) = (&platform.media, &layout.media_dir) {
        wanted.push(Wanted {
            kind: MountKind::Media,
            label: "Scraped media".into(),
            local: local(media),
            server: view.join(media_dir),
        });
    }
    let sort = node.retroarch_sort.unwrap_or(platform.retroarch_sort);
    let name = |id: &str| emulators.get(id).map_or(id.to_string(), |e| e.name.clone());

    // RetroArch cores, grouped the way RetroArch arranges their files on this machine.
    let cores: Vec<(&str, &str)> = node
        .uses
        .iter()
        .flat_map(|(system, ids)| ids.iter().map(move |id| (system.as_str(), id.as_str())))
        .filter(|(_, id)| emulators.get(id).is_some_and(|e| e.kind == Kind::RetroArch))
        .collect();
    for (kind, ra_root, canonical_root) in [
        (
            MountKind::Saves,
            &platform.retroarch_saves,
            settings.saves_root(),
        ),
        (
            MountKind::States,
            &platform.retroarch_states,
            settings.states_root(),
        ),
    ] {
        if kind == MountKind::States && !node.sync_states {
            continue;
        }
        let what = if kind == MountKind::Saves {
            "Saves"
        } else {
            "States"
        };
        // Saves go to the assigned folder; states always to the emulator's own.
        let folder = |id: &str| -> String {
            match kind {
                MountKind::Saves => settings.save_folder(id).to_string(),
                _ => id.to_string(),
            }
        };
        let ra_root = local(ra_root);
        let mut groups: BTreeMap<PathBuf, (String, BTreeSet<String>, BTreeSet<String>)> =
            BTreeMap::new();
        for (system, id) in &cores {
            let (local_dir, group_label) = match sort {
                RetroArchSort::Core => match emulators.get(id).and_then(|e| e.corename.clone()) {
                    Some(core) => (ra_root.join(&core), core),
                    None => {
                        warnings.push(format!("{}: no RetroArch core name known", name(id)));
                        continue;
                    }
                },
                RetroArchSort::Content => {
                    let dir = layout.folder(system).to_string();
                    (ra_root.join(&dir), format!("RetroArch, {dir}"))
                }
                RetroArchSort::None => (ra_root.clone(), "RetroArch".to_string()),
            };
            let entry = groups
                .entry(local_dir)
                .or_insert_with(|| (group_label, BTreeSet::new(), BTreeSet::new()));
            entry.1.insert(folder(id));
            entry.2.insert(name(id));
        }
        for (local_dir, (group_label, folders, names)) in groups {
            let names = names.into_iter().collect::<Vec<_>>().join(", ");
            if folders.len() > 1 {
                let folders = folders.into_iter().collect::<Vec<_>>().join(", ");
                warnings.push(match kind {
                    MountKind::Saves => format!(
                        "{group_label}: {names} share one RetroArch folder on this machine but are \
                         assigned different save folders ({folders}). Assign them one folder, or \
                         sort RetroArch's saves by core. Not mounted."
                    ),
                    _ => format!(
                        "{group_label}: {names} share one RetroArch state folder on this machine; \
                         states are kept per emulator, so sort RetroArch's states by core. Not mounted."
                    ),
                });
                continue;
            }
            let folder = folders.into_iter().next().expect("group has a core");
            let label = match sort {
                RetroArchSort::Core => format!("{what}: {names}"),
                _ => format!("{what}: {group_label} ({names})"),
            };
            wanted.push(Wanted {
                kind,
                label,
                local: local_dir,
                server: canonical_root.join(folder),
            });
        }
    }

    // Standalone and custom emulators: the platform's known paths per slot.
    let standalone: BTreeSet<&str> = node
        .uses
        .values()
        .flatten()
        .map(String::as_str)
        .filter(|id| !emulators.get(id).is_some_and(|e| e.kind == Kind::RetroArch))
        .collect();
    for id in standalone {
        for (kind, table, canonical) in [
            (
                MountKind::Saves,
                &platform.saves,
                settings.saves_root().join(settings.save_folder(id)),
            ),
            (
                MountKind::States,
                &platform.states,
                settings.states_root().join(id),
            ),
        ] {
            if kind == MountKind::States && !node.sync_states {
                continue;
            }
            let what = if kind == MountKind::Saves {
                "Saves"
            } else {
                "States"
            };
            let Some(slots) = table.get(id) else {
                if kind == MountKind::Saves {
                    warnings.push(format!(
                        "{}: RomServer doesn't know where it keeps saves on {}. Not mounted.",
                        name(id),
                        platform.label
                    ));
                }
                continue;
            };
            for (slot, template) in slots {
                let label = match slot.as_str() {
                    "" => format!("{what}: {}", name(id)),
                    slot => format!("{what}: {} {slot}", name(id)),
                };
                let server = match slot.as_str() {
                    "" => canonical.clone(),
                    slot => canonical.join(slot),
                };
                wanted.push(Wanted {
                    kind,
                    label,
                    local: local(template),
                    server,
                });
            }
        }
    }

    let (shares, _) = shares::effective(settings);
    let mounts = resolve(wanted, &shares, settings, &mut warnings);
    warn_nested(&mounts, &mut warnings);
    let options = mount_options(settings.network.protocol, node);
    Ok(MountPlan {
        node: node_name.to_string(),
        platform_label: platform.label.clone(),
        mounter: platform.mounter,
        units_dir: platform.units_dir().to_string(),
        protocol: settings.network.protocol,
        options,
        uid: node.uid,
        gid: node.gid,
        mounts,
        warnings,
        notes: platform.notes.clone(),
    })
}

/// Drops duplicate mount points (keeping the first) and resolves each server path to a remote.
fn resolve(
    wanted: Vec<Wanted>,
    shares: &BTreeMap<String, ShareInfo>,
    settings: &Settings,
    warnings: &mut Vec<String>,
) -> Vec<Mount> {
    let mut seen: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut out = Vec::new();
    for w in wanted {
        if let Some(server) = seen.get(&w.local) {
            if *server != w.server {
                warnings.push(format!(
                    "{}: {} is already mounted from {}. Not mounted twice.",
                    w.label,
                    w.local.display(),
                    server.display()
                ));
            }
            continue;
        }
        match shares::remote_for(
            &w.server,
            shares,
            &settings.network.server,
            settings.network.protocol,
        ) {
            Ok(remote) => {
                seen.insert(w.local.clone(), w.server.clone());
                out.push(Mount {
                    kind: w.kind,
                    label: w.label,
                    local: w.local,
                    server: w.server,
                    remote,
                });
            }
            Err(e) => warnings.push(match shares::sharing_advice(settings, shares, &w.server) {
                Some(advice) => format!("{}: {e:#}. Not mounted. {advice}", w.label),
                None => format!("{}: {e:#}. Not mounted.", w.label),
            }),
        }
    }
    out
}

/// A mount inside another mount works (systemd orders them), but its mount point is created
/// inside the outer mount's server folder, so say so.
fn warn_nested(mounts: &[Mount], warnings: &mut Vec<String>) {
    for inner in mounts {
        if let Some(outer) = mounts
            .iter()
            .filter(|o| inner.local != o.local && inner.local.starts_with(&o.local))
            .max_by_key(|o| o.local.components().count())
        {
            warnings.push(format!(
                "{} sits inside {}: its mount point is created as an empty folder in {}.",
                inner.local.display(),
                outer.local.display(),
                outer
                    .server
                    .join(inner.local.strip_prefix(&outer.local).unwrap())
                    .display()
            ));
        }
    }
}

fn mount_options(protocol: Protocol, node: &Node) -> String {
    match protocol {
        Protocol::Smb => {
            let auth = match &node.credentials {
                Some(file) => format!("credentials={file}"),
                None => "guest".to_string(),
            };
            format!(
                "{auth},uid={},gid={},iocharset=utf8,_netdev",
                node.uid, node.gid
            )
        }
        Protocol::Nfs => "_netdev".to_string(),
    }
}

/// Server folders the plan mounts that don't exist yet. A mount of a missing folder fails.
pub fn missing_server_folders(plan: &MountPlan) -> Vec<PathBuf> {
    plan.mounts
        .iter()
        .filter(|m| matches!(m.kind, MountKind::Saves | MountKind::States))
        .map(|m| m.server.clone())
        .filter(|p| !p.is_dir())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// `systemd-escape --path`.
pub fn systemd_escape_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    let trimmed: Vec<&str> = text.split('/').filter(|p| !p.is_empty()).collect();
    if trimmed.is_empty() {
        return "-".to_string();
    }
    let joined = trimmed.join("/");
    let mut out = String::new();
    for (i, byte) in joined.bytes().enumerate() {
        match byte {
            b'/' => out.push('-'),
            b'.' if i == 0 => out.push_str("\\x2e"),
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b':' | b'_' | b'.' => out.push(byte as char),
            _ => {
                let _ = write!(out, "\\x{byte:02x}");
            }
        }
    }
    out
}

/// Quotes a string for POSIX sh.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

struct UnitFiles {
    mount_name: String,
    mount_body: String,
    automount_name: String,
    automount_body: String,
}

fn unit_files(plan: &MountPlan, mount: &Mount) -> UnitFiles {
    let name = systemd_escape_path(&mount.local);
    let mount_unit = format!(
        "# Generated by RomServer for {node}: {label}\n\
         [Unit]\n\
         Description=RomServer: {label}\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Mount]\n\
         What={what}\n\
         Where={where_}\n\
         Type={fs}\n\
         Options={options}\n\
         TimeoutSec=30\n",
        node = plan.node,
        label = mount.label,
        what = mount.remote.what,
        where_ = mount.local.display(),
        fs = mount.remote.fs_type,
        options = plan.options,
    );
    let automount_unit = format!(
        "# Generated by RomServer for {node}: {label}\n\
         [Unit]\n\
         Description=RomServer automount: {label}\n\
         \n\
         [Automount]\n\
         Where={where_}\n\
         TimeoutIdleSec=0\n\
         \n\
         [Install]\n\
         WantedBy=remote-fs.target\n",
        node = plan.node,
        label = mount.label,
        where_ = mount.local.display(),
    );
    UnitFiles {
        mount_name: format!("{name}.mount"),
        mount_body: mount_unit,
        automount_name: format!("{name}.automount"),
        automount_body: automount_unit,
    }
}

/// One shell script that installs (or with `uninstall`, removes) every unit for the machine.
/// Existing local save files are moved aside and copied into the share without overwriting.
pub fn systemd_script(plan: &MountPlan, generated: &str) -> String {
    let mut s = String::new();
    let units_dir = &plan.units_dir;
    let _ = writeln!(
        s,
        "#!/bin/sh\n\
         # RomServer mounts for {node} ({platform}), generated {generated}.\n\
         #\n\
         #   sudo sh {file}            install and start the automounts\n\
         #   sudo sh {file} uninstall  stop and remove them (local folders are left alone)\n\
         #\n\
         # Automounts are the safety net: if the server is unreachable, using a mount point\n\
         # fails instead of quietly writing to the local disk underneath.\n\
         set -eu\n\
         UNITS_DIR={units_dir}\n\
         STAMP=$(date +%Y%m%d-%H%M%S)\n\
         [ \"$(id -u)\" = 0 ] || {{ echo 'run as root (sudo)' >&2; exit 1; }}\n",
        node = plan.node,
        platform = plan.platform_label,
        file = format_args!("romserver-{}.sh", plan.node),
        units_dir = sh_quote(units_dir),
    );
    let units: Vec<(&Mount, UnitFiles)> = plan
        .mounts
        .iter()
        .map(|m| (m, unit_files(plan, m)))
        .collect();
    let automounts: Vec<String> = units
        .iter()
        .map(|(_, u)| sh_quote(&u.automount_name))
        .collect();
    let all_units: Vec<String> = units
        .iter()
        .flat_map(|(_, u)| [sh_quote(&u.mount_name), sh_quote(&u.automount_name)])
        .collect();
    let _ = writeln!(
        s,
        "if [ \"${{1:-}}\" = uninstall ]; then\n\
         \x20   systemctl disable --now {automounts} 2>/dev/null || true\n\
         \x20   for u in {units}; do systemctl stop \"$u\" 2>/dev/null || true; rm -f \"$UNITS_DIR/$u\"; done\n\
         \x20   systemctl daemon-reload\n\
         \x20   echo 'RomServer mounts removed.'\n\
         \x20   exit 0\n\
         fi\n",
        automounts = automounts.join(" "),
        units = all_units.join(" "),
    );
    let _ = writeln!(
        s,
        "# Refuse to start while anything else mounts these paths (an old fstab line, say):\n\
         # two mounts on one path would hide each other.\n\
         CONFLICTS=0\n\
         check() {{\n\
         \x20   if findmnt -s -n --mountpoint \"$1\" >/dev/null 2>&1; then\n\
         \x20       echo \"/etc/fstab already mounts $1: remove that line, then run: systemctl daemon-reload; umount '$1'\" >&2\n\
         \x20       CONFLICTS=1\n\
         \x20   elif mountpoint -q \"$1\" 2>/dev/null && ! systemctl is-active -q \"$2\" 2>/dev/null; then\n\
         \x20       echo \"$1 is already mounted by something else: unmount it first\" >&2\n\
         \x20       CONFLICTS=1\n\
         \x20   fi\n\
         }}\n\
         \n\
         # prepare MOUNT_POINT IMPORT: make the mount point; set a non-empty one aside first.\n\
         IMPORTS=''\n\
         prepare() {{\n\
         \x20   if mountpoint -q \"$1\" 2>/dev/null; then\n\
         \x20       return\n\
         \x20   fi\n\
         \x20   if [ -d \"$1\" ] && [ -n \"$(ls -A \"$1\" 2>/dev/null)\" ]; then\n\
         \x20       mv \"$1\" \"$1.romserver-local-$STAMP\"\n\
         \x20       echo \"moved existing $1 aside to $1.romserver-local-$STAMP\"\n\
         \x20       if [ \"$2\" = import ]; then\n\
         \x20           IMPORTS=\"$IMPORTS\n$1\"\n\
         \x20       fi\n\
         \x20   fi\n\
         \x20   mkdir -p \"$1\"\n\
         \x20   chown {uid}:{gid} \"$1\" 2>/dev/null || true\n\
         }}\n",
        uid = plan.uid,
        gid = plan.gid,
    );
    for (mount, u) in &units {
        let _ = writeln!(
            s,
            "check {} {}",
            sh_quote(&mount.local.to_string_lossy()),
            sh_quote(&u.automount_name)
        );
    }
    let _ = writeln!(
        s,
        "if [ \"$CONFLICTS\" = 1 ]; then\n\
         \x20   echo 'Nothing was changed.' >&2\n\
         \x20   exit 1\n\
         fi\n"
    );
    for (mount, _) in &units {
        let import = if mount.kind.imports_local_files() {
            "import"
        } else {
            "keep"
        };
        let _ = writeln!(
            s,
            "prepare {} {import}",
            sh_quote(&mount.local.to_string_lossy())
        );
    }
    s.push('\n');
    for (_, u) in &units {
        for (name, body) in [
            (&u.mount_name, &u.mount_body),
            (&u.automount_name, &u.automount_body),
        ] {
            let _ = writeln!(
                s,
                "cat > \"$UNITS_DIR\"/{} <<'ROMSERVER_UNIT'\n{body}ROMSERVER_UNIT",
                sh_quote(name)
            );
        }
    }
    let _ = writeln!(
        s,
        "\nsystemctl daemon-reload\n\
         systemctl enable --now {automounts}\n\
         \n\
         # Copy saves that were on this machine into the share, never overwriting what's there.\n\
         # The originals stay in the .romserver-local-* folders until you delete them.\n\
         printf '%s\\n' \"$IMPORTS\" | while IFS= read -r dir; do\n\
         \x20   [ -n \"$dir\" ] || continue\n\
         \x20   cp -rn --preserve=timestamps \"$dir.romserver-local-$STAMP/.\" \"$dir/\" \\\n\
         \x20       && echo \"imported $dir\" || echo \"could not import $dir; originals kept\" >&2\n\
         done\n\
         echo 'RomServer mounts installed.'",
        automounts = automounts.join(" "),
    );
    s
}

/// Batocera/Knulli: no systemd. ROMs come from batocera-boot.conf (its own network share
/// support); saves are mounted by /userdata/system/custom.sh at boot.
pub fn batocera_script(plan: &MountPlan, generated: &str) -> String {
    let mut s = String::new();
    let roms = plan.mounts.iter().find(|m| m.kind == MountKind::Roms);
    let _ = writeln!(
        s,
        "#!/bin/bash\n\
         # RomServer mounts for {node} ({platform}), generated {generated}.\n\
         #\n\
         # 1. ROMs: Batocera mounts them itself. Edit batocera-boot.conf on the boot partition\n\
         #    (from the box: mount -o remount,rw /boot; nano /boot/batocera-boot.conf) and set:\n\
         #\n\
         #      sharedevice=NETWORK",
        node = plan.node,
        platform = plan.platform_label,
    );
    if let Some(roms) = roms {
        let (keyword, location) = match plan.protocol {
            Protocol::Smb => (
                "sharenetwork_smb1",
                roms.remote
                    .what
                    .trim_start_matches('/')
                    .replacen('/', ":", 1),
            ),
            Protocol::Nfs => ("sharenetwork_nfs1", roms.remote.what.clone()),
        };
        let auth = match plan.protocol {
            Protocol::Smb => ":username=YOUR_USER,password=YOUR_PASSWORD",
            Protocol::Nfs => "",
        };
        let _ = writeln!(s, "#      {keyword}=ROMS@{location}{auth}");
    }
    let _ = writeln!(
        s,
        "#\n\
         # 2. Saves: copy this file to the box and run it once (bash romserver-{node}.sh). It\n\
         #    installs /userdata/system/romserver-mounts.sh and calls it from custom.sh.\n\
         #    Batocera has no automount: if the server is down at boot, saves go to local disk.\n\
         set -eu\n\
         HOOK=/userdata/system/romserver-mounts.sh\n\
         cat > \"$HOOK\" <<'ROMSERVER_HOOK'\n\
         #!/bin/bash\n\
         # Generated by RomServer for {node}. Mounts the shared save folders.",
        node = plan.node,
    );
    for mount in plan.mounts.iter().filter(|m| m.kind != MountKind::Roms) {
        let local = sh_quote(&mount.local.to_string_lossy());
        let _ = writeln!(
            s,
            "mkdir -p {local}\n\
             mountpoint -q {local} || mount -t {fs} {what} {local} -o {options} \\\n\
             \x20   || echo \"RomServer: could not mount {label}\" >&2",
            fs = mount.remote.fs_type,
            what = sh_quote(&mount.remote.what),
            options = sh_quote(&plan.options),
            label = mount.label,
        );
    }
    let _ = writeln!(
        s,
        "ROMSERVER_HOOK\n\
         chmod +x \"$HOOK\"\n\
         CUSTOM=/userdata/system/custom.sh\n\
         touch \"$CUSTOM\"\n\
         grep -qF \"$HOOK\" \"$CUSTOM\" || printf '\\n# RomServer save mounts\\n[ \"$1\" = start ] && %s\\n' \"$HOOK\" >> \"$CUSTOM\"\n\
         chmod +x \"$CUSTOM\"\n\
         echo 'Installed. Reboot, or run: bash /userdata/system/romserver-mounts.sh'"
    );
    s
}

/// The downloadable bundle for a machine.
pub fn script(plan: &MountPlan, generated: &str) -> String {
    match plan.mounter {
        Mounter::Systemd => systemd_script(plan, generated),
        Mounter::Batocera => batocera_script(plan, generated),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Share, Uses};

    #[test]
    fn escapes_paths_like_systemd() {
        let e = |p: &str| systemd_escape_path(Path::new(p));
        assert_eq!(
            e("/home/deus/.config/retroarch/saves/Snes9x"),
            "home-deus-.config-retroarch-saves-Snes9x"
        );
        assert_eq!(
            e("/home/deck/.var/app/org.libretro.RetroArch/config/retroarch/saves"),
            "home-deck-.var-app-org.libretro.RetroArch-config-retroarch-saves"
        );
        assert_eq!(
            e("/home/deus/ES-DE/downloaded_media/my game (1)"),
            "home-deus-ES\\x2dDE-downloaded_media-my\\x20game\\x20\\x281\\x29"
        );
        assert_eq!(e("/.hidden"), "\\x2ehidden");
        assert_eq!(e("/"), "-");
    }

    fn settings() -> Settings {
        let mut s: Settings = toml::from_str(include_str!("../romserver.example.toml")).unwrap();
        s.shares.insert(
            "ROMS".into(),
            Share {
                path: "/mnt/user/ROMS".into(),
                smb: true,
                nfs: false,
            },
        );
        s.shares.insert(
            "views".into(),
            Share {
                path: "/mnt/user/views".into(),
                smb: true,
                nfs: false,
            },
        );
        s.network.unraid_share_config = None;
        s.save_folders
            .insert("ra-snes9x".into(), "snes-saves".into());
        s
    }

    fn info(id: &str, name: &str, kind: Kind, corename: Option<&str>) -> EmulatorInfo {
        EmulatorInfo {
            id: id.into(),
            name: name.into(),
            kind,
            corename: corename.map(Into::into),
        }
    }

    fn catalog() -> Vec<EmulatorInfo> {
        vec![
            info("ra-snes9x", "Snes9x", Kind::RetroArch, Some("Snes9x")),
            info(
                "ra-mednafen_psx",
                "Beetle PSX",
                Kind::RetroArch,
                Some("Beetle PSX"),
            ),
            info(
                "ra-genesis_plus_gx",
                "Genesis Plus GX",
                Kind::RetroArch,
                Some("Genesis Plus GX"),
            ),
            info("pcsx2", "PCSX2", Kind::Standalone, None),
            info("dolphin", "Dolphin", Kind::Standalone, None),
        ]
    }

    fn plan(s: &Settings, node: &str) -> MountPlan {
        let infos = catalog();
        let index: HashMap<&str, &EmulatorInfo> =
            infos.iter().map(|e| (e.id.as_str(), e)).collect();
        plan_mounts(s, node, &index).unwrap()
    }

    fn find<'a>(p: &'a MountPlan, local: &str) -> &'a Mount {
        p.mounts
            .iter()
            .find(|m| m.local == Path::new(local))
            .unwrap_or_else(|| panic!("no mount at {local}: {:#?}", p.mounts))
    }

    #[test]
    fn native_es_de_matches_the_hand_written_fstab() {
        let mut s = settings();
        let desktop = s.nodes.get_mut("desktop").unwrap();
        desktop.home = Some("/home/deus".into());
        desktop.credentials = Some("/home/deus/.smbcredentials-carrier".into());
        let p = plan(&s, "desktop");
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        assert_eq!(
            find(&p, "/home/deus/.config/retroarch/saves/Snes9x")
                .remote
                .what,
            "//192.168.1.1/ROMS/saves/snes-saves"
        );
        assert_eq!(
            find(&p, "/home/deus/ROMs").remote.what,
            "//192.168.1.1/views/desktop/roms"
        );
        assert_eq!(
            find(&p, "/home/deus/ES-DE/downloaded_media").remote.what,
            "//192.168.1.1/views/desktop/downloaded_media"
        );
        assert_eq!(
            find(&p, "/home/deus/.config/PCSX2/memcards").server,
            Path::new("/mnt/user/ROMS/saves/ps2-saves")
        );
        assert!(
            p.options
                .starts_with("credentials=/home/deus/.smbcredentials-carrier,uid=1000")
        );
        let script = systemd_script(&p, "now");
        assert!(script.contains("home-deus-.config-retroarch-saves-Snes9x.automount"));
        assert!(script.contains("What=//192.168.1.1/ROMS/saves/snes-saves"));
    }

    #[test]
    fn retrodeck_sorts_retroarch_by_system_and_maps_dolphin_regions() {
        let mut s = settings();
        s.uses.insert("gc".into(), vec!["dolphin".into()]);
        s.systems.insert(
            "gc".into(),
            crate::settings::System {
                folder: "GC".into(),
            },
        );
        let mut node = Node::new("retrodeck", Default::default());
        node.uses = Uses::from([
            ("snes".into(), vec!["ra-snes9x".into()]),
            ("gc".into(), vec!["dolphin".into()]),
        ]);
        s.nodes.insert("deck".into(), node);
        let p = plan(&s, "deck");
        assert_eq!(
            find(&p, "/home/deck/retrodeck/saves/snes").server,
            Path::new("/mnt/user/ROMS/saves/snes-saves")
        );
        assert_eq!(
            find(&p, "/home/deck/retrodeck/saves/gc/dolphin/US").server,
            Path::new("/mnt/user/ROMS/saves/gc/GC/USA")
        );
        assert_eq!(find(&p, "/home/deck/retrodeck/roms").kind, MountKind::Roms);
    }

    #[test]
    fn one_unsorted_retroarch_folder_needs_one_save_folder() {
        let mut s = settings();
        let mut node = Node::new("emudeck", Default::default());
        node.uses = Uses::from([
            ("snes".into(), vec!["ra-snes9x".into()]),
            ("genesis".into(), vec!["ra-genesis_plus_gx".into()]),
        ]);
        s.nodes.insert("deck".into(), node);
        let p = plan(&s, "deck");
        assert!(
            p.warnings
                .iter()
                .any(|w| w.contains("assigned different save folders")),
            "{:?}",
            p.warnings
        );
        s.save_folders
            .insert("ra-genesis_plus_gx".into(), "snes-saves".into());
        let p = plan(&s, "deck");
        assert_eq!(
            find(
                &p,
                "/home/deck/.var/app/org.libretro.RetroArch/config/retroarch/saves"
            )
            .server,
            Path::new("/mnt/user/ROMS/saves/snes-saves")
        );
    }

    #[test]
    fn unexported_shares_and_unknown_paths_are_warnings_not_failures() {
        let mut s = settings();
        s.shares.remove("views");
        s.uses.insert(
            "psx".into(),
            vec!["ra-mednafen_psx".into(), "duckstation".into()],
        );
        let pi = s.nodes.get_mut("pi2").unwrap();
        pi.platform = "retropie".into();
        pi.uses.insert("psx".into(), vec!["duckstation".into()]);
        let p = plan(&s, "pi2");
        assert!(
            p.warnings
                .iter()
                .any(|w| w.starts_with("ROMs: no share contains"))
        );
        assert!(
            p.warnings
                .iter()
                .any(|w| w.contains("doesn't know where it keeps saves"))
        );
        assert!(
            p.mounts
                .iter()
                .any(|m| m.local == Path::new("/home/pi/RetroPie/saves/Snes9x"))
        );
    }

    #[test]
    fn batocera_gets_boot_config_and_custom_sh() {
        let mut s = settings();
        s.nodes.get_mut("pi2").unwrap().platform = "batocera".into();
        let p = plan(&s, "pi2");
        let script = script(&p, "now");
        assert!(script.contains("sharenetwork_smb1=ROMS@192.168.1.1:views/pi2/roms"));
        assert!(script.contains("/userdata/saves/megadrive"), "{script}");
    }
}
