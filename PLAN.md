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
| `watch` | inotify plus periodic full scan (inotify is unreliable for changes arriving via SMB/NFS/FUSE). |
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

Saves never get hard-linked per file, so atomic-rename saves can't split links.

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

Saves are **not** routed through views. RomServer generates systemd `.mount` + `.automount`
units for each client machine: one for its ROM view, plus one per emulator save directory,
mounting a canonical save folder **directly onto the emulator's local save path**. This is the
existing hand-written fstab pattern (`saves/snes-saves` → `~/.config/retroarch/saves/Snes9x`),
generated instead of hand-written. No server-side symlinks are needed.

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

Canonical store: `/share/saves/<emulator>/` (states: `/share/states/<emulator>/`). Optionally,
the user can point several emulators at one save folder (e.g. Snes9x and bsnes both write raw-SRAM
`<rom name>.srm`), so one save follows the game across emulators.

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
