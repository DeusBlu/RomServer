# rom_server — Plan

A small Rust service, packaged as a Docker container for hosts like Unraid, that keeps **one
canonical library** of ROMs, media and saves, and **projects** it into per-frontend views using
hard links. Each frontend machine (ES-DE, RetroDECK, RetroPie, Batocera, …) mounts **one** view
over the network with a single automount unit and sees exactly the folder structure and file
names it expects. Every byte on disk exists once.

This replaces the current setup of hand-built composite views in `/etc/fstab`.

## 1. Scope

| Data | How it reaches a view |
|---|---|
| ROMs (incl. `.cue`/`.bin`/`.chd`/`.m3u`) | Hard links, optionally renamed |
| Scraped media | Hard links, renamed to match the view's ROM names |
| Saves | **Not projected per file.** Canonical save directories are exposed in the view; clients symlink their emulator save dirs to them on the remote side (§5) |
| `gamelist.xml` | **Never mapped.** Each frontend owns its own gamelist |

## 2. Hard constraints

1. **One filesystem, one mount.** Hard links don't cross Docker bind mounts (`EXDEV`, even on the
   same host filesystem). The container gets a single mount, e.g. host `/mnt/user/games` →
   `/data`, containing the library and all views. Same rule as the TRaSH guides for the *arr stack.
2. **Unraid.** Single user share, never mix `/mnt/user/...` with `/mnt/diskN/...`. Verify link
   behaviour across the mover on the real box in Phase 0.
3. **Directories can't be hard-linked.** Views are real directory trees populated with linked
   files. Saves are the exception: they're exposed as directories (§5).
4. **Library files are never modified by the engine; views only change shape.** A view chooses
   which files appear and what they are called. (The one-time m3u migration in §6.1 was a
   deliberate, separate change to the library.)
5. **Disc sets stay flat.** In the library and every view, a `.m3u`, its disc cue sheets, and
   their `.bin` tracks share one folder. `.cue` `FILE` lines reference `.bin`s by exact name, so
   **`.bin` names are never changed**. The scanner enforces the layout; a violation is a plan
   error, not a silent breakage.

## 3. Architecture

```
  Canonical library (/data/library)
            │ scan
            ▼
  Catalog (SQLite) ── games, discs, files, media, inode/size/mtime,
            │          view_entry (every path the tool created)
            │ plan, per enabled profile
            ▼
  Profile adapters (es-de, retrodeck, batocera, retropie, …)
            │ desired-state manifest: view path → canonical file | directory
            ▼
  Diff vs. view on disk → apply (link / mkdir / unlink / rmdir), dry-run by default
```

Declarative and idempotent, like Terraform: running it twice does nothing the second time.

### Crates / modules

| Module | Responsibility |
|---|---|
| `scan` | Walk the library, record inode + size + mtime, and group files into games by parsing references: `.m3u` → `.cue` → `.bin` (cue `FILE` lines). Unreferenced files become single-file games. |
| `lint` | Check disc-set invariants, report only: dangling references, discs outside their `.m3u`'s folder, sets not in canonical form (§6.1: `(Disc N)` cue still has `.cue`, missing `.m3u`, gaps in disc numbers, lowercase `(disc N)`), `.bin`s with no cue. |
| `catalog` | SQLite (`rusqlite`). Source of truth for what exists and what the tool owns. |
| `profile` | Trait + one module per frontend. Pure function: catalog + options → manifest. No I/O. |
| `plan` | Diff manifest vs. disk; validate (name collisions, broken `.m3u` refs, cross-device). |
| `apply` | Execute plan; record `view_entry`. |
| `audit` | Detect drift: view links whose inode no longer matches canonical, unknown files in views. Report only in v1. |
| `watch` | inotify plus periodic full scan (inotify does see SMB writes on Unraid, through `/mnt/user`; the scan covers writes that bypass it). |
| `cli` | `scan`, `plan`, `apply`, `audit`, `profiles`. Web UI comes later. |

Config: one TOML file listing enabled views, their profile, and profile options.

### Safety rules

- The tool **only deletes paths recorded in `view_entry`**. Unknown files in a view are reported,
  never removed.
- It **never writes into the canonical library.**
- Every `apply` is preceded by a printable plan; `--dry-run` is the default.

## 3a. Container layout

The ROM share is **source data only**: views never go there, and only the Manage tools write to
it. Views get their own Unraid share. Both shares must be reachable through **one** container
mount, because a hard link between two bind mounts fails with `EXDEV` (tested 2026-10-25), even
when the disks are the same. So the container mounts their common parent:

```
host /mnt/user  → /mnt/user   (ONE mount; paths identical inside and out)
  /mnt/user/ROMS/              source share (settings: library_root)
    PSX/ PS2/ SegaCD/ …         canonical ROMs (existing folder names; mapped to short names)
    media/<system>/<type>/      canonical scraped media (already exists)
    saves/<emulator>/…          canonical saves
  /mnt/user/views/             dedicated "views" share (settings: views_root)
    <node>/  e.g. desktop/, steam-machine/, pi2/   one per machine; what it mounts

host /mnt/user/appdata/romserver → /config (separate mount; nothing here is ever hard-linked)
  data/romserver.sqlite
  settings/romserver.toml      flat-file options: enabled views, profile options, system map
  logs/  undo/
```

Why the DB isn't on the ROM share: SQLite on Unraid's `/mnt/user` FUSE layer has a long history
of corruption reports; the Unraid community advice is appdata on a cache-only pool. The appdata
share is the conventional home for container state and gets backed up by the usual appdata
backup plugin. The views, by contrast, **must** be on the ROM share because hard links can't
leave it.

Clients mount `//unraid/views/<node>` (or the NFS equivalent): one automount unit each.

**Verified on Unraid 2026-10-25** (throwaway container via the `unraid` docker context, mounting
`/mnt/user`): links from `ROMS` into a separate top-level share work; renamed links (a `.cue`
linked without its extension) work; each link lands on the **source file's disk** (disk1 and disk2
sources both tested); `nlink` is reported correctly. shfs reports its own **synthetic inode
numbers** (e.g. `649081317821810278`, vs the real `21527037542` on `/mnt/disk2`), consistent across
all names of a file. So the catalog must always stat through `/mnt/user` and never mix in
`/mnt/diskN` paths. A new top-level folder also creates empty directories on other disks;
harmless. Test share removed afterwards; source link counts back to 1. Still untested: mover
behaviour (cache → array) and the clients' SMB view of the `views` share.

Original checklist: hard links work **between two user shares** through
`/mnt/user` (shfs has to put the link on the source file's disk even if the `views` share's
disk settings differ); Global Share Settings' hard-link support tunable is on; the mover keeps
links. If cross-share links fail, the fallback is one parent share containing both `ROMS/` and
`views/` as sibling folders.

Mounting all of `/mnt/user` gives the container broad access. A narrower option is a parent
share holding just `ROMS/` and `views/`, at the cost of moving the ROM share once.

### App shape

One static Rust binary (musl, distroless/scratch image) that is both the engine and the web UI:
- `axum` HTTP server, server-rendered HTML + `htmx` (no JS build step), assets embedded in the
  binary.
- `rusqlite` (bundled) for the catalog; `serde` + `toml` for settings.
- Same binary exposes CLI subcommands (`scan`, `plan`, `apply`, `audit`, `prep`) for scripting
  and tests.
- Runs as root, because Unraid's share settings (`/boot/config/shares`, on the FAT boot stick)
  are readable by root only, and hands everything it creates to `PUID:PGID` (defaults 99:100,
  `nobody:users`, as Unraid's shares use) with `UMASK` 000, so it stays writable over SMB. All
  creation goes through `src/ownership.rs` (create folders and chown exactly the new ones; new
  files chowned). `docker-entrypoint.sh` sets the umask and brings `/config` in line. History:
  the first builds created root-owned 755 folders nobody else could write in; dropping to
  `nobody` instead lost access to the share settings. If the settings are mounted but unreadable,
  the Settings page and each machine's mounts say so instead of reporting every share missing.
- Long operations (scan, apply, prep) run as background jobs with progress streamed to the UI;
  every mutating job shows its dry-run plan first and needs a confirm click.

## 4. Canonical library layout

Frontend-neutral. **Default is flat per system** (how the primary user keeps it, and easiest to
script against). Per-game subfolders are also supported; the only rule is §2.4.

```
/data/library/
  roms/<system>/                            flat: every .m3u, .cue, .bin, .chd side by side
      Final Fantasy VII (USA).m3u
      Final Fantasy VII (USA) (Disc 1).cue
      Final Fantasy VII (USA) (Disc 1).bin
      …
  roms/<system>/<Game>/…                    optional per-game folders, same rule inside
  (saves found beside ROMs — .srm, .mcr, .state — are classified as saves, never projected as ROMs)
  rom_server.overrides.toml                 per-game overrides (display name, hide, disc order),
                                            kept out of the ROM folders so they stay clean
  media/<system>/<game-id>/<type>.<ext>     box2d, screenshot, marquee, video, manual, …
  saves/<system>/…                          real directories, written directly by clients
  bios/
/data/views/<view-name>/                    generated, owned by the tool, one export per client
/data/.rom_server/catalog.sqlite, logs/
```

Systems are keyed by **ES-DE short names** (`psx`, `snes`, `gc`); profiles map from those.
Support a one-time import of an existing ES-DE-style library into this layout.

## 5. Saves

Saves are **projected like ROMs** (decided 2026-11-08, superseding "saves are never hard-linked
per file"): the library keeps them in its own shape, and each machine's view hard-links them into
the shape that machine's emulators expect. The data's shape never follows a frontend's. Tested on
supertux over SMB to Unraid with RetroArch (Snes9x): an in-game save rewrote the linked file in
place (same inode, links=2, library copy changed with it); a brand-new save arrived as a real file
in the view (links=1), which the monitor brings into the library.

### Where frontends put saves (researched 2026-10-25)

Save layout is decided by the **emulator** and then **arranged differently by each frontend**:

| Frontend | Layout | Example |
|---|---|---|
| EmuDeck | `Emulation/saves/<emulator>/{saves,states}` | `saves/duckstation/saves`, `saves/pcsx2/saves`, `saves/retroarch/saves` |
| RetroDECK | `retrodeck/saves/<system>/<emulator>/…`, states in `retrodeck/states/<system>/<emulator>` | `saves/ps2/pcsx2/memcards`, `saves/gc/dolphin/{EU,JP,US}`, `saves/wii/dolphin` |
| Batocera | `/userdata/saves/<system>/` for RetroArch, emulator dirs for standalones | `saves/dolphin-emu` (Dolphin's whole user dir) |
| RetroPie | RetroArch default: save beside the ROM | `roms/psx/Game.srm` |
| ES-DE alone | none; whatever the launched emulator does | — |

Sources: EmuDeck's save-management docs, the RetroDECK PCSX2/Dolphin wiki guides, the Batocera
wiki, and the EmuDeck issue tracker on RetroArch sort options.

### Design: nodes, and generated mount units

Saves are routed through views. RomServer generates systemd `.mount` + `.automount` units for
each client machine: one for its ROM view, and one for each outermost folder its emulators keep
saves or states in, mounting that folder's place in the view (`views/<node>/machine/<the
machine's own path>`, e.g. `views/supertux/machine/home/deus/.config/retroarch/saves/Snes9x`).
The view mirrors the machine's paths, so a save folder inside another (RetroDECK keeps
DuckStation's memory cards inside `saves/psx`) is covered by the outer mount. `src/saves.rs`
works out the folders and which library folders fill each.

Config model (people use the same emulators everywhere, so emulators are chosen once, globally):

```
Emulators I use   (global list: snes9x, duckstation, pcsx2, dolphin, …)
Nodes             (machines: "desktop", "steam-machine")
  └─ Frontend + install type  (es-de native, emudeck Flatpak, …) → which view it mounts
```

Each registered node gets a **checklist of the global emulators** it actually has (a Pi 2 might
tick only the 8/16-bit ones). RomServer generates save mounts only for the ticked emulators,
resolving each one's local save path from the node's frontend and install type.

**Views are per machine.** Each node gets its own view, `views/<node>/`, built from its frontend
profile plus the systems its ticked emulators cover. Two machines with identical settings
still get **two separate views**, so one can change later without any logic to split them
apart. Links are only directory entries, so the duplication costs nothing.

Canonical store: **one save folder per system**, `saves/<system>/` (by media group, so tg16
uses pcengine's), shared by every emulator used for that system. Where their formats match they
see the same save (Snes9x and bsnes both write raw-SRAM `<rom name>.srm`); where they don't, each
keeps its own file beside the others. Per system and emulator the user can name another folder
instead (e.g. an existing `snes-saves`). The folder is chosen per **(system, emulator) pair**: an
emulator used for several systems keeps each system's saves apart (PicoDrive's Sega CD saves go
to `saves/segacd`, never `saves/genesis`). States are never shared between emulators:
`states/<emulator>/`, shared by every system that emulator runs, since a state only loads in the
emulator that made it, and systems sharing a format share states by design. (Corrected
2026-11-01: the first build defaulted to one folder per emulator, which kept emulators from ever
sharing saves unless assigned by hand. Corrected 2026-11-08: an emulator used for several systems
had kept one folder, its first system's, which put Sega CD saves in Genesis's folder.)

**Projection.** Each machine folder is filled with links to every file in the library folders
that feed it. RetroArch writes to `saves/<core name>/` when sorting by core (its default) and to
`saves/<content folder>/` when sorting by content folder (RetroArch's `runloop.c` appends the
content folder, then the core name if both are on). So on a machine sorting by core, one core used
for Genesis and Sega CD gets one folder showing both `saves/genesis/*` and `saves/segacd/*`; on a
RetroDECK machine (by content folder) each system's folder shows its own. Nobody has to change
RetroArch's settings. Two library files that would land on one name in a machine folder
(case-insensitively, as SMB sees it) show the first and warn. Standalone emulators declare which
system each save slot holds (`[emulators.dolphin] systems` in `assets/platforms.toml`: GameCube's
`GC/*` slots are gc's, `Wii` is wii's); a slot is filled only where the machine uses the emulator
for that system. Projections live in the same view as the ROMs, so a full Apply keeps them, and
the monitor refreshes them every pass, so a save made on one machine reaches the others.

**New saves (the monitor's ingest).** A file in a machine folder that isn't RomServer's link was
written by the machine. If one library folder feeds that folder, it goes there; if several do
(one core for two systems), the file's game name decides, matched against the names that machine
lists for each system (RetroArch names saves after the launched file). It is copied into the
library through the library's own path (so it lands on the library's disks), checked, and the
view's file replaced by a link; the next pass links it into every other machine. Nothing is ever
guessed or overwritten: a name matching no game, or games on both systems, stays in the view as an
alert; a library file of the same name with different bytes is kept beside the machine's, both
untouched (the same bytes just become a link). An emulator that saves by writing a new file
replaces RomServer's link: that is the newest save, so it replaces the library's, and the old one
goes to the undo folder. Deleting a save on a machine only removes that projection (the next pass
puts it back); saves are deleted from RomServer.

Rules and edge cases:
- **Compatibility data comes later.** A future emulator database records each emulator's save
  format and naming and warns "these emulators can't share X" (e.g. Beetle PSX `Game.srm` vs
  DuckStation's per-game `.mcd` names). v1 trusts the user.
- **States are never shared across emulators**: separate `states/<system>/<emulator>/` folders.
- **Mounting over a directory hides whatever is already in it.** Before generating a unit,
  RomServer's install script imports existing local saves into the canonical folder (never
  overwriting; conflicts are kept side by side), then mounts.
- **Automount is the safety net.** If the server is unreachable, accessing the mount point
  fails instead of silently writing to the local disk underneath. Use `.automount`, never plain
  `nofail` mounts.
- **Unit names must be `systemd-escape --path` of the mount point** (Flatpak paths like
  `~/.var/app/org.libretro.RetroArch/...` included). Generate them; never hand-build.
- **Credentials stay on the node** (`credentials=` points at an existing local file). RomServer
  never stores or emits SMB passwords.
- The RetroArch sort toggles (§ edge case 3) are part of the emulator definition, so the
  generated mount path matches where that client's RetroArch actually writes.
- Output per node: a bundle of unit files plus an install script (`systemctl daemon-reload`,
  enable the automounts), downloadable from the web UI.

### Save edge cases

1. **Shared-container saves.** A PCSX2 `.ps2` memory card holds every game played on it. The
   same goes for Dolphin GC cards per region, the Wii NAND, and RPCS3/Ryujinx/Yuzu `savedata`
   trees keyed by title ID. Xemu keeps saves inside a whole **HDD image**. Two devices playing
   at once means **last writer wins and the other device's progress is lost**. Accepted as a
   small edge case ("available everywhere" ≠ "available everywhere at once"). Mitigation: the UI
   labels these "one device at a time", and RomServer keeps **versioned backups** of
   shared-container saves whenever they change (last N per file, size-capped; small memcards
   yes, multi-GB HDD images like Xemu's opt-in only).
2. **EmuDeck's save folders are often symlinks themselves**, pointing back into local Flatpak
   dirs (RetroArch, Dolphin, PPSSPP, Citra, Ryujinx). Mounting a view over `Emulation/` doesn't
   redirect those saves; the emulator still writes locally. The client must point the emulator
   (or its Flatpak dir) into the mount, and EmuDeck's updater may undo that. This is the
   save guardian's main job.
3. **RetroArch save names follow the launched file's name** (the `.m3u` name for multi-disc), and
   its "sort saves by core / by content directory" toggles change the path. They must match on
   every client. RetroDECK sorts by content directory (`saves/psx/`); EmuDeck doesn't. The
   profile records which layout its frontend uses.
4. **Saves beside ROMs** (RetroPie default; your PSX folder already has `.srm`/`.mcr`/`.state`).
   A view's ROM folders are real directories, so a save written there lands **only in that view**.
   Options: configure RetroArch's `savefile_directory` on those clients (preferred), or have
   `audit` detect new save files in views and offer `adopt` (move them into canonical + link
   back). The library's existing beside-ROM saves get a one-time move into `saves/` via the
   Manage tools.
5. **Same game, different emulators.** Beetle PSX `.srm` vs DuckStation per-game `.mcd`, LRPS2 vs
   PCSX2 standalone: separate saves. Canonical-by-emulator keeps them apart instead of pretending
   they're one.
6. **Save states are emulator-version specific.** Clients on different emulator versions will
   fail to load each other's states. In-game saves are safe. Note this in the UI.

### Save monitoring: two features

RetroArch decides where a save goes with two separate settings: `savefiles_in_content_dir`
(**Write Saves to Content Directory**, or no `savefile_directory` set) puts it beside the ROM, and
`sort_savefiles_enable` (**Sort Saves into Folders by Core Name**) adds the core subfolder. With
per-core mounts, the first leaves direct evidence on the server; the second leaves none.

**1. Save monitor (automatic, direct evidence).** A background job inside RomServer walks every
machine's view and compares it with what RomServer created (`view_entry`). It reacts to
changes: an inotify watch on every machine's `views/<node>/machine/` tree runs a pass once
writes have been quiet for 2 seconds. Samba on Unraid writes through the same `/mnt/user` mount
the container sees, so SMB writes do raise events (checked 2026-11-08 from supertux: create,
write, close and delete all arrived). A full pass every 10 minutes is the backstop, for writes
that bypass `/mnt/user` (a share set to exclusive access) or a dropped event queue. A file
RomServer didn't create is an **alert**:
- a save-type file (`.srm`, `.sav`, `.state*`, …) beside a ROM → banner: *"steam-machine wrote
  `Chrono Trigger (USA).srm` into its view: Write Saves to Content Directory is on (or no save
  folder is set) on that machine."* Action: **adopt** (move it into the shared save folder for the
  machine's core for that system, then remove it from the view).
- any other unknown file (scraped media, gamelist, …) → a quieter notice.

Alerts are stored in SQLite (kind, machine, path, first seen, dismissed) and shown as a banner on
every page until resolved (the file is gone or adopted) or dismissed.

**2. Check button ("my save didn't carry over").** The user picks a machine and a game. RomServer
runs the monitor immediately, then searches for the save by name (RetroArch names it after the
launched file, so the `.m3u` name for multi-disc games):
- found in the shared folder for the ticked core → it reached the server; the other machine hasn't
  re-read it, or launched with a different core;
- found in a view → the monitor's alert and its adopt action;
- found under **another core's** folder → the game was launched with a different core;
- **found nowhere** → a troubleshooting list, most likely first: "Sort Saves into Folders by Core
  Name" is off (the save is in the root of the client's local saves folder); the save mount was
  down; that core isn't ticked for that machine (no mount); the machine's install type
  (Flatpak vs native) is wrong.

### Optional: client save guardian

The weak point is on the client. If the mount drops, or an emulator/frontend update replaces
the save symlink with a real directory, saves land on local disk and silently diverge. A small
optional client agent (same Rust workspace, separate binary, systemd user service) would:

1. Check that each configured save path is still a symlink into the mounted view, and that the
   mount is live.
2. If the mount is down: trigger the automount / remount and wait.
3. If a symlink was replaced by a real directory: move the local files into the share (keeping
   both copies on a name or mtime conflict, never overwriting), then restore the symlink.
4. Log every action; notify if it had to move anything.

Server-side counterpart: `audit` flags save directories that haven't been written for unusually
long while the matching ROMs have been played, as a hint that a client is writing locally.

## 6. Profiles

Fill in from primary docs in Phase 0.

| Question | ES-DE | RetroDECK | RetroPie | Batocera |
|---|---|---|---|---|
| ROM root & system folders | `ROMs/<system>` | `roms/<system>` | `roms/<system>` | `roms/<system>` |
| Media layout | `downloaded_media/<system>/<type>/<rom-basename>.<ext>` | same as ES-DE | per gamelist; convention `images/` beside ROMs | `images/`, `videos/` beside ROMs |
| Multi-disc | `.m3u` *directory* containing discs + `.m3u` (still flat inside, so §2.4 holds) | same as ES-DE | flat `.m3u` + hidden discs | flat `.m3u` |
| Save dirs | emulator-defined | `saves/<system>/`, `states/` | RetroArch savefile dir | `saves/<system>/` |

Per-view options, e.g. for PSX:

- `disc_format`: `chd` | `cue` | `prefer-chd`
- `multi_disc`: `m3u-directory` | `m3u-flat` | `discs-only`
- `show_single_disc_m3u`: bool
- `rename`: game-level template, e.g. `{title} ({region})`; applies to the `.m3u` and `.cue`
  names only, never to `.bin`s (§2.5)

### 6.1 Multi-disc projection modes

Per view, for each multi-disc set:

**Canonical form (the library).** A multi-disc set is marked by `(Disc N)` in its names. Its cue
sheets are stored **without an extension**, and a `Game.m3u` beside them lists those extensionless
names in disc order. The library is self-consistent: read directly, it shows one game per set.
The `.m3u` name is the shared prefix before `(Disc N)`, plus any trailing tags shared by every
disc (e.g. `Driver 2 (USA) (v1.1).m3u`); per-disc tags such as `(GDI)`/`(NOD)` are dropped.
This matches the names existing RetroArch saves already use.

Migrated 2026-10-25: 63 cue sets (157 cues: 60 PSX, 1 PS2, 1 Saturn, 1 SegaCD) and 13 PS2 ISO
sets (27 ISOs), 76 playlists in all (confirmed by `romserver scan`). Undo log: `migration-undo-2026-10-25.tsv`. Any disc format works the same way; the
`.m3u` content is just names, and the emulator reads the file's contents, not its extension.

#### Library tools ("Manage")

The only commands allowed to modify the library. All of them run per system, are dry-run by
default, validate everything before touching anything, and write an undo log.

| Tool | Does |
|---|---|
| **Prep Folders** | The opinionated default: Flatten, then normalise disc sets (below). |
| **Flatten ROMs** | Move every file out of per-game folders into the system folder. |
| **Folder Sort ROMs** | The reverse: one plain folder per game (named after the `.m3u` or single file), holding its discs, tracks and any saves beside them. |
| **Check Cues** (`romserver cues [--fix]`, built) | Check every cue sheet's `FILE` references (extensionless ones included). Fix only when exactly one file fits: case-only mismatch, same stem with another extension, or, for single-track cues, the file named after the cue. Otherwise report. A fixed cue is written as a new file and renamed into place; the original goes to the undo folder. |

Frontend layout never comes from the library; ES-DE's `Game.m3u/` folders are built by
projection (§6.1). The engine accepts either library layout. The invariant is "a set's files
share one folder", not "everything is flat".

Every one of these is a `rename()` on the same filesystem, so **inodes don't change and existing
view links stay valid**; the next scan just records the new paths. Only a regenerated `.m3u` is
a new inode, and re-planning relinks it.

Guards:
- **Name comparisons must be case-insensitive and case-only renames go through a temp name.**
  The library may be reached over SMB (case-insensitive) even though Unraid's disks are
  case-sensitive; `(disc 1)` → `(Disc 1)` looked like a collision on the CIFS mount.
- **Flatten refuses on name collisions** (two folders each containing `disc1.cue`, or generic
  track names). Fixing those means renaming `.bin`s and rewriting cue contents, which these tools
  never do. Report the collision and leave the set alone.
- **Systems whose games are directories are never flattened or sorted** (PS3, Wii U, PC, …). The
  profile/system definition marks them `folder-native`.

#### Disc-set normalisation (step 2 of Prep Folders)

1. Find sets: files sharing a prefix before `(Disc N)`, any disc extension (`.cue`, `.iso`,
   `.chd`, …) or already extensionless.
2. Validate first, before touching anything: every disc of a set is in one folder, disc numbers
   are contiguous from 1, every cue `FILE` reference resolves, and no rename target exists.
   A set that fails is skipped and reported, never partially migrated.
3. Strip the extension of the **disc entry file only**, meaning the file the `.m3u` lists and the
   frontend would otherwise show: the `.cue`, or a single-file image like `.iso`. Track files
   (`.bin`) always keep their extension; the cue sheet references them by exact name.
4. Delete the set's existing `.m3u` and write a fresh one (extensionless names, disc order).
   Generated files aren't worth repairing. An `.m3u` that doesn't belong to any detected set
   (hand-made playlist) is reported, not deleted.
5. Idempotent: on an already-prepared set the regenerated `.m3u` is byte-identical, so the run
   changes nothing.

The `.m3u` name rule must stay deterministic forever: RetroArch names saves after the content
file, so changing the rule would orphan every multi-disc save.

| Mode | View contains | Frontend sees |
|---|---|---|
| `m3u` (default) | everything under its library name: `Game.m3u`, extensionless cues, `.bin`s | one entry |
| `discs` | no `.m3u`; each cue linked as `Game (Disc N).cue`, `.bin`s | one entry per disc (RetroPie style) |
| `m3u-directory` | folder `Game.m3u/` containing the `.m3u`, extensionless cues, `.bin`s | one entry (ES-DE native) |

Projection is just two decisions per set: include the `.m3u` or not, and add `.cue` to disc
names or not. They're coupled: adding `.cue` breaks the `.m3u`'s references, so `discs` never
includes it.

Risks to verify in Phase 0:

1. ~~Emulators may pick a disc parser by file extension.~~ Settled: the user has run
   extensionless cue sheets through `.m3u` in every emulator they've used for ~20 years;
   emulators parse the content, not the extension. Still open: whether PCSX2 supports `.m3u`
   for the two PS2 sets.
2. **`.bin` is a scanned PSX extension in ES-DE and RetroPie.** Since `.bin`s can't be renamed,
   in the flat modes they still show up as entries unless the frontend's system extension list
   excludes `.bin`. `m3u-directory` avoids this for ES-DE.

Media names always follow the view's ROM name, so a renamed game keeps its art.

## 7. Phases

**Milestone 1 status (2026-10-25): built.** Rust crate `romserver`, a CLI with `scan`, `plan`,
`apply` and `cues`. Settings in TOML (`romserver.example.toml`), ownership in SQLite, an ES-DE
profile with all three multi-disc modes, and 10 tests. Real-library scan over CIFS takes ~5 s.
On Unraid, the container (`Dockerfile`, loaded via the `unraid` context) projected PSX + PS2 +
Genesis + SegaCD into a scratch share: 10,088 links, a re-plan was a no-op, the `m3u` → `discs`
switch removed 255 and created 180, and **0 files in ROMS were created or modified**. Scratch
share and image removed afterwards.

**Milestone 2 status (2026-10-25): built.** `romserver serve` (the container's default command,
port 8080): axum + maud server-rendered pages + htmx 2.0.4, with no JS build step. htmx is embedded
in the binary but not committed: `tools/fetch_htmx.sh` downloads the pinned version and checks its
SHA-256 (run it once after cloning; the Dockerfile runs it). The UI lives in `src/web/`, one module
per page. Pages:
Overview, Library (scan; check and fix cues), Emulators (the global list; removing one also unticks
it on every machine), Machines (add; frontend, multi-disc mode and emulator checklist; preview →
confirm → apply; clear view; remove only once the view is empty). The UI rewrites
`romserver.toml` atomically, but comments in it are lost. Apply and cue-fix refuse if the
recomputed plan differs from the preview's counts. One file-changing operation runs at a time.
Login was added afterwards (see the emulator catalog note below). Verified on Unraid through HTTP: 9,056 links applied,
re-preview "up to date", remove refused while links existed, clear removed 9,056; cleaned up.

**Emulator catalog (2026-10-25).** `tools/build_catalog.py` builds `assets/emulators.json` from
ES-DE's Linux `es_systems.xml` and `libretro-core-info` (both MIT): 195 systems and 294 emulators,
174 of them RetroArch cores. Core ids are `ra-<core file stem>` and carry `corename`, which is
RetroArch's save folder with sort-by-core (e.g. `Snes9x`). Standalone ids come from ES-DE's
`%EMULATOR_X%` variable (`duckstation`). Launchers (shortcuts, Wine, Proton, UMU) are excluded.
The catalog is embedded in the binary and loaded into SQLite (`emu_system`, `emu_emulator`,
`emu_support`) once per run. User choices stay in the TOML: `[uses]` system → emulator ids
(ticking one emulator under several systems = "I use it for all of these"), plus
`[custom_emulators.custom-<slug>]` with a name and systems. Each machine's `uses` is a subset of
the global one; unticking globally prunes machines. A machine's view = the systems it has ticks
for. Login: Lantern's scheme (`src/auth.rs`).

**Library and Settings in the UI (2026-10-31).** Systems are no longer hand-written TOML: the
Library page lists the library's real subfolders that aren't systems yet (a share can hold
backups and archives too, so nothing is assumed) and the user pairs one with a system from the
catalog's ES-DE list. Removing a system drops its emulator choices everywhere (custom emulators
left with no systems go too); views lose it at their next apply, and no files are deleted. The
Settings page holds the paths (`library_root`, `views_root`, optional `saves_root`, default
`library_root/saves`), `[network]` (server address clients use, `smb` | `nfs`) and the shares.
Shares are read from Unraid's `/boot/config/shares/*.cfg` when it is mounted read-only into the
container (default `/unraid/shares`; `shareExport`/`shareExportNFS` = `e` or `eh` means exported;
configs whose folder is gone are skipped), and `[shares.<name>]` entries add to or override them
by name. A server path maps to the deepest share containing it: SMB `//server/<share>/<rest>`,
NFS `server:<path>`. The page shows what every machine's view and the save folders would be
mounted from, and warns when the views folder is on a different filesystem than the library.
A first-time user isn't expected to know what to share: when a path can't be mounted, the page
(and each machine's mount list) says what to do in Unraid, e.g. "`views` has no Unraid share
settings yet: open Shares → views, set SMB Security Settings → Export to Yes". A share Unraid
created from a new top-level folder (the first view apply creates `views`) has no `.cfg` until
its settings are saved, so it is invisible until then, and the advice covers that case.
No `views` share exists on the Unraid box yet; it has to be created there before views can be
mounted.

**Platforms and mount bundles (2026-10-31).** A machine now picks a **platform** from
`assets/platforms.toml` (embedded): ES-DE with native or Flatpak emulators, EmuDeck, RetroDECK,
RetroPie, Batocera/Knulli, Lakka. It replaces §6's table as the source of truth and records,
per platform: the view layout (folder renames such as `genesis` → `megadrive`), where ROMs and
media mount, RetroArch's save/state folders and how it sorts them (`core` | `content` | `none`),
and each standalone emulator's save paths. Paths were taken from EmuDeck's and RetroDECK's own
setup scripts, not guessed: mounts go on the **real** folders (EmuDeck's
`Emulation/saves/<emu>/*` are symlinks into Flatpak dirs; RetroDECK's real folders are under
`retrodeck/saves`, with the emulator paths linked to them). Standalone emulators declare
canonical **slots** so one canonical folder lines up across platforms that name things
differently (Dolphin: `GC/EUR|USA|JAP`, `Wii`; RetroDECK calls them `gc/dolphin/EU|US|JP`).
Views are now `views/<node>/roms/<system>` with `views/<node>/downloaded_media` beside it, so
each is one mount (already-applied views relink on their next apply). Settings: `frontend`
became `platform` (old files still load; `es-de` is a platform id), machines gained `home`,
`base`, `uid`/`gid`, `credentials` (a path on the machine; RomServer never sees the contents),
`retroarch_sort` and `sync_states`. Save folders: `[save_folders.<system>]` assigns emulator id
→ named folder under `saves_root` (default: the system's folder, see §5); emulators on one
folder share saves when formats match. States always go to `states_root/<emulator id>`. RetroArch
cores follow the machine's sort mode: per core name, per system folder, or one folder; since
2026-11-08 each such folder is a projection in the view, filled from every library folder that
feeds it (§5). The Machines page
shows every mount (local path ← remote), warnings, platform notes, offers to create missing
save folders in the view (empty folders only; the monitor also creates them), and downloads `romserver-<node>.sh`: systemd
`.mount` + `.automount` units (names via `systemd-escape --path` rules) and an installer that
refuses to start while fstab or another mount already uses a path. Since 2026-11-08 (after it
moved a real local save folder aside on supertux and reported a `cp -n` that copied nothing as
"imported") it only writes units and creates empty mount points (missing parents included,
owned by the machine's user): it never moves, copies or deletes a file, and stops before
changing anything if a mount point holds files (empty folders, like a frontend's ready-made
system folders, are fine), naming the folder and saying to move the files out, install, then copy
them into the mounted folder so the monitor brings them in. It enables with `--quiet`, prints
each mount as `local <- remote` and says plainly that nothing was moved, copied or deleted. On
SteamOS (detected by `/usr/lib/rauc/atomic-update-keep.conf`) it lists the `.automount` units in
`/etc/atomic-update.conf.d/romserver-<node>.conf`, since SteamOS's own keep list covers `.mount`
units and enable links but not `.automount` (untested through a real OS update yet).
`uninstall` removes every unit carrying the machine's marker line, including ones left by an
earlier platform, plus the keep file. Batocera gets its
`batocera-boot.conf` ROMS line plus a `custom.sh` hook that mounts the saves (no automount
there). Generated units pass `systemd-analyze verify`.

**Media (2026-11-01).** No frontend's layout is canonical. Where frontends keep media differs
on four axes: location (ES-DE `~/ES-DE/downloaded_media`, EmuDeck
`~/Emulation/tools/downloaded_media`, RetroDECK `retrodeck/ES-DE/downloaded_media`, RetroArch
`thumbnails/<playlist>`, muOS `info/catalogue`, all a tree of their own; Batocera/Knulli
`roms/<system>/images|videos|manuals`, Pegasus `<rom dir>/media/<game>/`, Skyscraper's
ROM-folder mode `roms/<system>/media/`, all inside the ROM folders), naming (per-type folders,
one folder with suffixes such as Batocera's `-thumb`/`-image`/`-marquee`, per-game folders),
type names (`covers` = `-thumb` = `boxFront` = `Named_Boxarts`) and discovery (file names vs.
paths in `gamelist.xml`).

So the library keeps media **frontend-neutral, one folder per game**:
`media/<system>/<game>/<type>.<ext>`, types `box-front`, `box-back`, `box-3d`,
`physical-media`, `screenshot`, `title-screen`, `logo` (ES-DE's "marquees" folder holds logos),
`marquee`, `fanart`, `mix-image`, `video`, `manual`. Each layout in `assets/platforms.toml` maps
target patterns to types, first available wins. **Disc sets are one game**: one folder named
after the set, with art that belongs to one disc only in `<game>/disc-N/` (kept only where it
differs from the game's). `m3u` views use the game's folder; in `discs` mode each disc looks in
its `disc-N/` first, then the game's. (Names checked against Skyscraper's frontend
modules): ES-DE-family views get `downloaded_media/<system>/covers/<name>.png` etc. as a tree
mounted separately; Batocera views get `images/<name>-thumb.png`, `-image`, `-marquee`,
`-wheel`, `videos/<name>-video.mp4` **beside the ROMs**; RetroPie views get Skyscraper's
`media/covers/…` inside the ROM folders. No gamelists are generated: those frontends show the
files once their own gamelist points there. Media names follow the names each view lists.

**Media groups.** Systems that are one platform under several names share one media folder:
`media/<group>/`, where the group comes from ES-DE's `<platform>` tag (now carried in
`assets/emulators.json`): tg16 uses pcengine's, megacd/megacdjp use segacd's, sfc/snesna use
snes's. `[media_groups]` in `platforms.toml` joins what ES-DE keeps apart but is the same
hardware (megadrive → genesis, famicom → nes) and keeps ES-DE's catch-all scraper platforms
(arcade, pc, pcwindows, android) separate, because their members are different systems.

**Existing media (initial import).** Media from before RomServer is imported per system from the
Library page. It looks in the system's media folder (and folders of its group's other names)
and in sub-folders of its ROM folder (Batocera's `images/`, Skyscraper's `media/`, an old
`Images/`). Folder-based games carry images of their own (PS3's `PS3_GAME/ICON0.PNG`,
`PIC1.PNG`), so game-data folders (`PS3_GAME`, `PS3_UPDATE`, `PS3_EXTRA`, `USRDIR`) are never
entered, and inside a ROM folder a file only counts as media if it is named after a game or sits
in a folder named for media (`images`, `media`, `covers`, `screenshots`, `boxart`, …); anything
else there is ignored without asking. Each file is read through every known layout's patterns, then matched to a game
(single games, disc sets and each disc; a disc's art belongs to its set). A set scraped once per
disc is folded into one game: per game and kind, the game's art is what the library already has,
else the set's own, else the lowest disc's; other discs' byte-identical copies are duplicates
(moved to the undo folder), ones that differ go to `disc-N/`. A scrape from a machine showing
discs fills the game's folder while it lacks that kind, otherwise the disc's own. Real PS2
preview: 10,397 recognised, 164 duplicates (every set's disc 2+ art matched disc 1's), none
needing input. Recognised files are imported without asking. The rest
are listed for a decision, per file or for a whole folder at once: **rename and re-import**
(name the game and the kind) or **discard** (moved to the undo folder; re-scrape instead), or
leave. Typical reasons: named after a game but the kind is unknown, no such game, or the library
already has that kind for that game. Everything is validated before anything moves; imports
are renames inside the library (inode kept, never overwrites), with an undo log; every view gets
the additions afterwards. Read-only previews of the real library: SNES 9,056, PCEngine 1,021,
Saturn 330, Sega CD 234 files recognised, none needing input. Not run yet.

**New media (scraping is a deliberate task).** Scraping happens on a machine, into its own
view: the main data set holds only real files, views only projections, so a scrape leaves real
files in a view until the user runs **New media added** for that machine. It lists the real
files in the view (anything that isn't still RomServer's link, by recorded inode), reads each
through the machine's layout (pattern → type, name → game), and on confirm **copies** each into
`media/<system>/<game>/<type>.<ext>`: written through the library's own path, so the bytes land
on the library's disks (a hard link would leave them on the views share's), checked for size,
placed without overwriting. Then it replaces the view's file with a link to the copy, records it,
and updates every machine's view with **additions and relinks only** (no preview needed: nothing
can be removed). Re-scraped art replaces the library's (newest wins); the old file is kept in
the undo folder. Mounts never change; the media just appears.

**Frontend fixes: ES-DE and cue tracks (2026-11-01).** ES-DE's defaults list `.bin` for every
system that takes cue sheets (19 of them, PSX among them), so each cue's track files show up as
games of their own. RomServer can't and doesn't touch a machine's ES-DE config; it advises. Each
machine page shows which of its systems need it, with how many track files ES-DE lists. On
these systems a `.bin` is always a cue sheet's track: a `.bin` without one is a disc rip missing
its cue, or a legacy cartridge dump (the cartridge systems here have their own extensions now,
like Genesis's `.md`), which isn't supported for now. Such a game is **damaged**: it is reported
(Library scan, each view's preview warnings, the machine page) as "damaged, missing its cue
sheet; it won't show up on any machine", and left out of every view, so no frontend lists an
entry that can't play. On the real library this found one (Saturn:
`Mystaria The Realms of Lore (U).bin`, probably a bad rip). The page then explains the hand fix (an entry in ES-DE's `custom_systems/es_systems.xml` replaces the built-in
one of the same name: ES-DE's own entry minus `.bin .BIN`), and offers to do it: the user pastes
their custom systems file and gets it back fixed (their entries kept; for a system they
customised, only `.bin` is removed from its extensions), or says they have none and gets
RomServer's default. Nothing is generated before they choose. ES-DE's entries come from
`assets/esde_cue_systems.xml`, written by `tools/build_catalog.py`, and are used only for that
fix. The library rule itself (which systems are cue systems, and finding lone `.bin` files) is
frontend-neutral and lives in `scan.rs`: it reads each system's `bin_is_cue_track` flag in
`assets/emulators.json`, which the builder sets where ES-DE lists both `.cue` and `.bin`.

**Recorded inodes.** `view_entry` now stores the linked file's (device, inode) at link time. An
owned path is relinked or removed only while it still holds that file. A different file there
was written on the machine (a scraper replaces files by delete-and-create): it is reported for
New media added, never overwritten. Before this, the planner's relink would have put the old art
back over a fresh re-scrape that was the only copy. Links recorded before the change count as
RomServer's while still linked elsewhere.

**Save monitor and check (2026-10-31): built as §5 describes.** A background task walks every
machine's view when its save folders change, and every 10 minutes regardless (since 2026-11-08;
it was each minute; a pass waits while an apply holds the lock, since an apply creates files
before recording them) and stores files RomServer didn't create in the `alert` table (node,
path, kind `save` | `other`, first seen, dismissed); rows vanish when the file does. Every page
loads a banner (htmx, `/saves/banner`) while save alerts are open. The **Saves** page lists them
with **Move to <folder>**, offering the save folders of the emulators that machine has for the
file's system, RetroArch cores first. The move is a rename, falling back to copy, size check,
then remove across filesystems, and it **never overwrites**: if the folder already has that name,
nothing moves. Only files the monitor reported as saves can be moved. Other files (media,
gamelists) are listed quietly and never deleted. **My save didn't carry over**: pick a machine
and a game (autocompleted from the names that machine's frontend lists, which RetroArch also
names saves after); RomServer searches every save folder and every view and says which of
these it is: reached a folder this machine uses, stuck in a view, under another
emulator's folder, or nowhere (with the troubleshooting list from §5).

**View rule: views are immutable/additive.** View files are never written; any change is
remove + fresh link.

**Status 2026-10-25:** local hard-link behaviour verified on btrfs: projecting one inode under
different names (with and without `.cue`) works, library renames and moves keep view links
intact, in-place writes show through every name, and a link across two bind mounts fails with
`EXDEV`, confirming the single-mount container rule. The engine always runs on the host that owns
the disks, so links never cross the network. Remaining: repeat inside the container on Unraid.

**Phase 0 — Spike (local first).** Dev data set: `/mnt/carrier/ROMS` (CIFS mount of the Unraid
`ROMS` share). PSX there is flat, multi-disc sets are already migrated to canonical form (§6.1), and a few saves
sit next to the ROMs. Steps:
1. Probe whether `link()` works through the CIFS mount, in a throwaway directory on the share.
2. Test the §6.1 risks with real emulators on one multi-disc game.
3. Settle the save-directory mechanism (§5) and fill in the §6 table.
4. Then repeat the link probe in a container on Unraid with a single bind mount.

**Phase 1 — Engine + CLI + ES-DE ROMs.** Scan, catalog, manifest, plan/diff, apply, ownership,
dry-run. Integration tests against a fake library in a tempdir.

**Phase 2 — PSX and media.** `.m3u`/cue parsers, disc-set grouping over a flat folder, `lint`,
the §6.1 projection modes. Media projection with renaming.

**Phase 3 — Saves + audit.** Save directory exposure, drift and unknown-file reporting, client
setup output (automount unit, save symlinks).

**Phase 4 — More profiles.** RetroDECK (cheap after ES-DE), Batocera, RetroPie, others.

**Phase 5 — Packaging.** Dockerfile (static musl binary, tiny image), Unraid template
(PUID/PGID, single `/data` mount), scheduled re-plan, small web UI.

**Phase 6 — Client save guardian (optional).** See §5.

## 8. Open questions

1. **Scraped media written by a frontend into its view** shows up as an unknown file. Report only,
   or offer an `adopt` command that moves it into the canonical library and relinks?
2. **ROM files dropped into a view** by hand: same question as 1.
3. **Save-directory mechanism** over the actual export protocol (§5).
4. **Cross-emulator saves** (e.g. Beetle PSX vs. DuckStation memcards) are out of scope unless the
   formats are identical.
