//! Client mount bundles: what each machine mounts (its view, its emulators' save folders) and
//! the files that set it up: systemd `.mount` + `.automount` units with an install script, or
//! Batocera's boot config and `custom.sh`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::emulators::EmulatorInfo;
use crate::platforms::{self, Mounter, VIEW_ROMS};
use crate::saves;
use crate::settings::{Node, Protocol, Settings};
use crate::shares::{self, Remote, ShareInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountKind {
    Roms,
    Media,
    Saves,
    States,
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
    // Saves and states: each machine folder is mounted from its place in the view, where
    // RomServer projects the library's saves into the shape this machine's emulators expect.
    // The view mirrors the machine's paths, so a folder inside another is covered by the outer
    // one's mount.
    let (dirs, save_warnings) = saves::save_dirs(settings, node_name, emulators)?;
    warnings.extend(save_warnings);
    let mut outermost: Vec<&saves::SaveDir> = Vec::new();
    let mut by_depth: Vec<&saves::SaveDir> = dirs.iter().collect();
    by_depth.sort_by_key(|d| d.local.components().count());
    for dir in by_depth {
        if outermost.iter().any(|o| dir.local.starts_with(&o.local)) {
            continue;
        }
        outermost.push(dir);
    }
    outermost.sort_by(|a, b| a.local.cmp(&b.local));
    for dir in outermost {
        wanted.push(Wanted {
            kind: match dir.kind {
                saves::SaveKind::Saves => MountKind::Saves,
                saves::SaveKind::States => MountKind::States,
            },
            label: dir.label.clone(),
            local: dir.local.clone(),
            server: view.join(dir.view_path()),
        });
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

/// The line every unit RomServer writes for a machine starts with; `uninstall` finds them by it,
/// so units from an earlier platform or plan are removed too.
fn unit_marker(node: &str) -> String {
    format!("# Generated by RomServer for {node}:")
}

/// One shell script that installs (or with `uninstall`, removes) every unit for the machine.
/// It writes unit files and creates empty mount points, nothing else: it never moves, copies
/// or deletes a file, and refuses to mount over a folder that has files in it.
pub fn systemd_script(plan: &MountPlan, generated: &str) -> String {
    let mut s = String::new();
    let units: Vec<(&Mount, UnitFiles)> = plan
        .mounts
        .iter()
        .map(|m| (m, unit_files(plan, m)))
        .collect();
    let file = format!("romserver-{}.sh", plan.node);
    let _ = writeln!(
        s,
        "#!/bin/sh\n\
         # RomServer mounts for {node} ({platform}), generated {generated}.\n\
         #\n\
         #   sudo sh {file}            install and start the automounts\n\
         #   sudo sh {file} uninstall  stop and remove every RomServer mount for {node}\n\
         #\n\
         # It writes a .mount and an .automount unit for each folder below into {units_dir},\n\
         # creates any missing mount point (owned by {uid}:{gid}), and on SteamOS lists the\n\
         # .automount units in {keep} so OS updates keep them.\n\
         # It never moves, copies or deletes a file, and nothing on the server is changed.\n\
         #\n\
         # On this machine  <-  from the server",
        node = plan.node,
        platform = plan.platform_label,
        units_dir = plan.units_dir,
        uid = plan.uid,
        gid = plan.gid,
        keep = keep_file(&plan.node),
    );
    for (mount, _) in &units {
        let _ = writeln!(
            s,
            "#   {}  <-  {}",
            mount.local.display(),
            mount.remote.what
        );
    }
    let _ = writeln!(
        s,
        "#\n\
         # Automounts are the safety net: if the server is unreachable, using a mount point\n\
         # fails instead of quietly writing to the local disk underneath.\n\
         set -eu\n\
         UNITS_DIR={units_dir}\n\
         KEEP={keep}\n\
         MARKER={marker}\n\
         [ \"$(id -u)\" = 0 ] || {{ echo 'run as root (sudo)' >&2; exit 1; }}\n\
         \n\
         if [ \"${{1:-}}\" = uninstall ]; then\n\
         \x20   REMOVED=0\n\
         \x20   for path in \"$UNITS_DIR\"/*.automount \"$UNITS_DIR\"/*.mount; do\n\
         \x20       [ -f \"$path\" ] && grep -qF \"$MARKER\" \"$path\" || continue\n\
         \x20       unit=$(basename \"$path\")\n\
         \x20       case \"$unit\" in *.automount) systemctl disable --quiet \"$unit\" 2>/dev/null || true ;; esac\n\
         \x20       systemctl stop \"$unit\" 2>/dev/null || true\n\
         \x20       rm -f \"$path\"\n\
         \x20       REMOVED=$((REMOVED + 1))\n\
         \x20   done\n\
         \x20   rm -f \"$KEEP\"\n\
         \x20   systemctl daemon-reload\n\
         \x20   echo \"Removed $REMOVED RomServer unit files for {node}. Local folders were left alone.\"\n\
         \x20   exit 0\n\
         fi\n\
         \n\
         # Check everything before changing anything.\n\
         PROBLEMS=0\n\
         check() {{\n\
         \x20   if findmnt -s -n --mountpoint \"$1\" >/dev/null 2>&1; then\n\
         \x20       echo \"/etc/fstab already mounts $1: remove that line, then run: systemctl daemon-reload; umount '$1'\" >&2\n\
         \x20       PROBLEMS=1\n\
         \x20   elif mountpoint -q \"$1\" 2>/dev/null; then\n\
         \x20       systemctl is-active -q \"$2\" 2>/dev/null && return\n\
         \x20       echo \"$1 is already mounted by something else: unmount it first\" >&2\n\
         \x20       PROBLEMS=1\n\
         \x20   elif [ -d \"$1\" ]; then\n\
         \x20       # Empty folders (a frontend's ready-made system folders) are fine; files aren't.\n\
         \x20       FILES=$(find \"$1\" -mindepth 1 ! -type d 2>/dev/null | wc -l)\n\
         \x20       if [ \"$FILES\" -gt 0 ]; then\n\
         \x20           echo \"$1 has $FILES file(s) in it, which the mount would hide. Move them somewhere else and run this again; then copy them into $1 and RomServer brings them into the library without overwriting anything.\" >&2\n\
         \x20           PROBLEMS=1\n\
         \x20       fi\n\
         \x20   fi\n\
         }}",
        units_dir = sh_quote(&plan.units_dir),
        keep = sh_quote(&keep_file(&plan.node)),
        marker = sh_quote(&unit_marker(&plan.node)),
        node = plan.node,
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
        "if [ \"$PROBLEMS\" = 1 ]; then\n\
         \x20   echo 'Nothing was changed.' >&2\n\
         \x20   exit 1\n\
         fi\n\
         \n\
         # Missing mount points (and any missing folders above them) belong to the machine's user.\n\
         mountpoint_dir() {{\n\
         \x20   [ -d \"$1\" ] && return\n\
         \x20   mountpoint_dir \"$(dirname \"$1\")\"\n\
         \x20   mkdir \"$1\"\n\
         \x20   chown {uid}:{gid} \"$1\"\n\
         }}",
        uid = plan.uid,
        gid = plan.gid,
    );
    for (mount, _) in &units {
        let _ = writeln!(
            s,
            "mountpoint -q {local} 2>/dev/null || mountpoint_dir {local}",
            local = sh_quote(&mount.local.to_string_lossy())
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
    // SteamOS replaces /etc on every update, keeping only what its keep lists name; .mount
    // units and the enable links are on its own list, .automount units aren't.
    let _ = writeln!(
        s,
        "\nKEPT=0\n\
         if [ -f /usr/lib/rauc/atomic-update-keep.conf ]; then\n\
         \x20   mkdir -p \"$(dirname \"$KEEP\")\"\n\
         \x20   cat > \"$KEEP\" <<'ROMSERVER_KEEP'"
    );
    for (_, u) in &units {
        let _ = writeln!(s, "{}/{}", plan.units_dir, u.automount_name);
    }
    let automounts: Vec<String> = units
        .iter()
        .map(|(_, u)| sh_quote(&u.automount_name))
        .collect();
    let _ = writeln!(
        s,
        "ROMSERVER_KEEP\n\
         \x20   KEPT=1\n\
         fi\n\
         \n\
         systemctl daemon-reload\n\
         systemctl enable --now --quiet {automounts}\n\
         \n\
         echo 'RomServer mounts for {node} are installed:'",
        automounts = automounts.join(" "),
        node = plan.node,
    );
    for (mount, _) in &units {
        let _ = writeln!(
            s,
            "echo {}",
            sh_quote(&format!(
                "  {}  <-  {}",
                mount.local.display(),
                mount.remote.what
            ))
        );
    }
    let _ = writeln!(
        s,
        "[ \"$KEPT\" = 1 ] && echo \"SteamOS: listed in $KEEP, so OS updates keep them.\"\n\
         echo 'Nothing was moved, copied or deleted, and nothing on the server was changed.'"
    );
    s
}

/// SteamOS's keep list for a machine's units.
fn keep_file(node: &str) -> String {
    format!("/etc/atomic-update.conf.d/romserver-{node}.conf")
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
    use crate::emulators::Kind;
    use crate::platforms::RetroArchSort;
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
        // Saves are mounted from the view, which mirrors the machine's own paths.
        assert_eq!(
            find(&p, "/home/deus/.config/retroarch/saves/Snes9x")
                .remote
                .what,
            "//192.168.1.1/views/desktop/machine/home/deus/.config/retroarch/saves/Snes9x"
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
            Path::new("/mnt/user/views/desktop/machine/home/deus/.config/PCSX2/memcards")
        );
        // … filled from the library's folders for each system and emulator.
        assert_eq!(
            libraries(&s, "desktop", "/home/deus/.config/retroarch/saves/Snes9x"),
            ["/mnt/user/ROMS/saves/snes-saves"]
        );
        assert_eq!(
            libraries(&s, "desktop", "/home/deus/.config/PCSX2/memcards"),
            ["/mnt/user/ROMS/saves/ps2-saves"]
        );
        assert!(
            p.options
                .starts_with("credentials=/home/deus/.smbcredentials-carrier,uid=1000")
        );
        let script = systemd_script(&p, "now");
        assert!(script.contains("home-deus-.config-retroarch-saves-Snes9x.automount"));
        assert!(script.contains(
            "What=//192.168.1.1/views/desktop/machine/home/deus/.config/retroarch/saves/Snes9x"
        ));
    }

    #[test]
    fn the_installer_only_writes_units_and_never_touches_files() {
        let mut s = settings();
        let desktop = s.nodes.get_mut("desktop").unwrap();
        desktop.home = Some("/home/deus".into());
        let p = plan(&s, "desktop");
        let script = systemd_script(&p, "now");
        // It is valid sh.
        let path = std::env::temp_dir().join(format!("romserver-test-{}.sh", std::process::id()));
        std::fs::write(&path, &script).unwrap();
        let syntax = std::process::Command::new("sh")
            .arg("-n")
            .arg(&path)
            .output()
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(
            syntax.status.success(),
            "{}",
            String::from_utf8_lossy(&syntax.stderr)
        );
        // No moving, copying or deleting files; a folder with files in it stops the install.
        for forbidden in ["mv ", "cp ", "rm -rf", "romserver-local"] {
            assert!(!script.contains(forbidden), "{forbidden} in:\n{script}");
        }
        assert!(script.contains("which the mount would hide"));
        assert!(script.contains("systemctl enable --now --quiet"));
        // SteamOS keeps the automount units through updates.
        assert!(script.contains("/etc/atomic-update.conf.d/romserver-desktop.conf"));
        assert!(
            script.contains(
                "/etc/systemd/system/home-deus-.config-retroarch-saves-Snes9x.automount\n"
            )
        );
        // Uninstall finds every unit generated for this machine by its marker line.
        assert!(script.contains("'# Generated by RomServer for desktop:'"));
        for mount in &p.mounts {
            assert!(script.contains(&format!(
                "  {}  <-  {}",
                mount.local.display(),
                mount.remote.what
            )));
        }
    }

    /// The library folders that fill one of a machine's save folders.
    fn libraries(s: &Settings, node: &str, local: &str) -> Vec<String> {
        let infos = catalog();
        let index: HashMap<&str, &EmulatorInfo> =
            infos.iter().map(|e| (e.id.as_str(), e)).collect();
        let (dirs, _) = saves::save_dirs(s, node, &index).unwrap();
        let dir = dirs
            .iter()
            .find(|d| d.local == Path::new(local))
            .unwrap_or_else(|| panic!("no save folder {local}: {dirs:#?}"));
        dir.libraries()
            .iter()
            .map(|p| p.display().to_string())
            .collect()
    }

    #[test]
    fn retrodeck_sorts_retroarch_by_system_and_maps_dolphin_regions() {
        let mut s = settings();
        for (system, folder) in [("gc", "GC"), ("wii", "Wii")] {
            s.uses.insert(system.into(), vec!["dolphin".into()]);
            s.systems.insert(
                system.into(),
                crate::settings::System {
                    folder: folder.into(),
                },
            );
        }
        let mut node = Node::new("retrodeck", Default::default());
        node.uses = Uses::from([
            ("snes".into(), vec!["ra-snes9x".into()]),
            ("gc".into(), vec!["dolphin".into()]),
        ]);
        s.nodes.insert("deck".into(), node);
        let p = plan(&s, "deck");
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        assert_eq!(
            find(&p, "/home/deck/retrodeck/saves/snes").server,
            Path::new("/mnt/user/views/deck/machine/home/deck/retrodeck/saves/snes")
        );
        assert_eq!(
            libraries(&s, "deck", "/home/deck/retrodeck/saves/snes"),
            ["/mnt/user/ROMS/saves/snes-saves"]
        );
        assert_eq!(
            libraries(&s, "deck", "/home/deck/retrodeck/saves/gc/dolphin/US"),
            ["/mnt/user/ROMS/saves/gc/GC/USA"]
        );
        assert_eq!(find(&p, "/home/deck/retrodeck/roms").kind, MountKind::Roms);
        // Dolphin isn't used for Wii here, so its Wii slot isn't mounted.
        assert!(
            !p.mounts
                .iter()
                .any(|m| m.local == Path::new("/home/deck/retrodeck/saves/wii/dolphin"))
        );

        // Used for Wii too: Wii saves come from Wii's folder, not GameCube's.
        let deck = s.nodes.get_mut("deck").unwrap();
        deck.uses.insert("wii".into(), vec!["dolphin".into()]);
        let p = plan(&s, "deck");
        find(&p, "/home/deck/retrodeck/saves/wii/dolphin");
        assert_eq!(
            libraries(&s, "deck", "/home/deck/retrodeck/saves/wii/dolphin"),
            ["/mnt/user/ROMS/saves/wii/Wii"]
        );
    }

    #[test]
    fn one_core_for_two_systems_is_one_folder_fed_by_both() {
        let mut s = settings();
        let mut node = Node::new("es-de", Default::default());
        node.home = Some("/home/deus".into());
        node.sync_states = true;
        node.uses = Uses::from([
            ("genesis".into(), vec!["ra-genesis_plus_gx".into()]),
            ("segacd".into(), vec!["ra-genesis_plus_gx".into()]),
        ]);
        s.nodes.insert("desk".into(), node);

        // Sorted by core (RetroArch's default), the core's one folder shows both systems'
        // saves; the library keeps them apart.
        let p = plan(&s, "desk");
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        let saves = "/home/deus/.config/retroarch/saves/Genesis Plus GX";
        find(&p, saves);
        assert_eq!(
            libraries(&s, "desk", saves),
            [
                "/mnt/user/ROMS/saves/genesis",
                "/mnt/user/ROMS/saves/segacd"
            ]
        );
        // States are kept per emulator: one library folder for both.
        assert_eq!(
            libraries(
                &s,
                "desk",
                "/home/deus/.config/retroarch/states/Genesis Plus GX"
            ),
            ["/mnt/user/ROMS/states/ra-genesis_plus_gx"]
        );

        // Sorted by content folder, each system's folder is fed by its own library folder.
        s.nodes.get_mut("desk").unwrap().retroarch_sort = Some(RetroArchSort::Content);
        let platforms = platforms::bundled();
        let layout = platforms.layout(platforms.platform("es-de").unwrap());
        for system in ["genesis", "segacd"] {
            let local = format!(
                "/home/deus/.config/retroarch/saves/{}",
                layout.folder(system)
            );
            assert_eq!(
                libraries(&s, "desk", &local),
                [format!("/mnt/user/ROMS/saves/{system}")]
            );
        }
    }

    #[test]
    fn one_unsorted_retroarch_folder_is_fed_by_every_core() {
        let mut s = settings();
        let mut node = Node::new("emudeck", Default::default());
        node.uses = Uses::from([
            ("snes".into(), vec!["ra-snes9x".into()]),
            ("genesis".into(), vec!["ra-genesis_plus_gx".into()]),
        ]);
        s.nodes.insert("deck".into(), node);
        let p = plan(&s, "deck");
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        let local = "/home/deck/.var/app/org.libretro.RetroArch/config/retroarch/saves";
        assert_eq!(
            find(&p, local).server,
            Path::new("/mnt/user/views/deck/machine").join(&local[1..])
        );
        assert_eq!(
            libraries(&s, "deck", local),
            [
                "/mnt/user/ROMS/saves/genesis",
                "/mnt/user/ROMS/saves/snes-saves"
            ]
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
        // Saves are mounted from the view too, so without the views share they can't be.
        assert!(
            p.warnings
                .iter()
                .any(|w| w.starts_with("Saves: Snes9x") && w.contains("Not mounted")),
            "{:?}",
            p.warnings
        );
        assert!(p.mounts.is_empty(), "{:#?}", p.mounts);
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
