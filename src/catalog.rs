//! SQLite state: which view paths RomServer created (so it never deletes anything it didn't
//! make), and the emulator catalog as reference data.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::emulators::{CatalogFile, EmulatorInfo, Kind};

pub struct Catalog {
    conn: Connection,
}

/// A view path RomServer created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owned {
    /// The library file it links to.
    pub source: PathBuf,
    /// (device, inode) of that file when linked. None for links made before this was recorded.
    pub identity: Option<(u64, u64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    /// A save or save state beside a ROM: the machine writes saves into its content folder.
    Save,
    /// Anything else (scraped media, a gamelist, a dropped ROM).
    Other,
}

impl AlertKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AlertKind::Save => "save",
            AlertKind::Other => "other",
        }
    }

    fn from_str(s: &str) -> Self {
        if s == "save" {
            AlertKind::Save
        } else {
            AlertKind::Other
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub node: String,
    /// Relative to the node's view root.
    pub path: PathBuf,
    pub kind: AlertKind,
    pub first_seen: i64,
    pub dismissed: bool,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            crate::ownership::create_dir_all(dir)
                .with_context(|| format!("create {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        crate::ownership::own(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS view_entry (
                 node   TEXT NOT NULL,
                 path   TEXT NOT NULL,  -- relative to the node's view root
                 source TEXT NOT NULL,  -- absolute library path it links to
                 PRIMARY KEY (node, path)
             );
             CREATE TABLE IF NOT EXISTS emu_system (
                 id   TEXT PRIMARY KEY,
                 name TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS emu_emulator (
                 id       TEXT PRIMARY KEY,
                 name     TEXT NOT NULL,
                 kind     TEXT NOT NULL,  -- retroarch | standalone
                 corename TEXT            -- RetroArch save folder, cores only
             );
             CREATE TABLE IF NOT EXISTS emu_support (
                 emulator TEXT NOT NULL,
                 system   TEXT NOT NULL,
                 PRIMARY KEY (emulator, system)
             );
             CREATE TABLE IF NOT EXISTS meta (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             -- Files in a view that RomServer didn't create (the save monitor).
             CREATE TABLE IF NOT EXISTS alert (
                 node       TEXT NOT NULL,
                 path       TEXT NOT NULL,     -- relative to the node's view root
                 kind       TEXT NOT NULL,     -- save | other
                 first_seen INTEGER NOT NULL,  -- unix seconds
                 dismissed  INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (node, path)
             );",
        )?;
        // The linked file's identity at link time: tells RomServer's own links (even stale ones)
        // apart from files a machine wrote at the same path. Older databases lack the columns.
        let has_identity = conn
            .prepare("SELECT 1 FROM pragma_table_info('view_entry') WHERE name = 'src_ino'")?
            .exists([])?;
        if !has_identity {
            conn.execute_batch(
                "ALTER TABLE view_entry ADD COLUMN src_dev INTEGER;
                 ALTER TABLE view_entry ADD COLUMN src_ino INTEGER;",
            )?;
        }
        Ok(Self { conn })
    }

    /// Makes the alert table match what the monitor just found: new files are added, files that
    /// are gone are forgotten, and existing rows keep their first-seen time and dismissal.
    pub fn sync_alerts(&mut self, found: &[Alert], now: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS seen (node TEXT, path TEXT); DELETE FROM seen;",
        )?;
        for a in found {
            let path = utf8(&a.path)?;
            tx.execute(
                "INSERT OR IGNORE INTO alert (node, path, kind, first_seen) VALUES (?1, ?2, ?3, ?4)",
                params![a.node, path, a.kind.as_str(), now],
            )?;
            tx.execute(
                "INSERT INTO seen (node, path) VALUES (?1, ?2)",
                params![a.node, path],
            )?;
        }
        tx.execute(
            "DELETE FROM alert WHERE NOT EXISTS
                 (SELECT 1 FROM seen WHERE seen.node = alert.node AND seen.path = alert.path)",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn alerts(&self) -> Result<Vec<Alert>> {
        let mut stmt = self.conn.prepare(
            "SELECT node, path, kind, first_seen, dismissed FROM alert ORDER BY node, path",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Alert {
                node: r.get(0)?,
                path: PathBuf::from(r.get::<_, String>(1)?),
                kind: AlertKind::from_str(&r.get::<_, String>(2)?),
                first_seen: r.get(3)?,
                dismissed: r.get::<_, i64>(4)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn dismiss_alert(&self, node: &str, path: &Path) -> Result<()> {
        self.conn.execute(
            "UPDATE alert SET dismissed = 1 WHERE node = ?1 AND path = ?2",
            params![node, utf8(path)?],
        )?;
        Ok(())
    }

    pub fn forget_alert(&self, node: &str, path: &Path) -> Result<()> {
        self.conn.execute(
            "DELETE FROM alert WHERE node = ?1 AND path = ?2",
            params![node, utf8(path)?],
        )?;
        Ok(())
    }

    /// view path → what RomServer linked there, for everything this node's view owns.
    pub fn owned(&self, node: &str) -> Result<BTreeMap<PathBuf, Owned>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, source, src_dev, src_ino FROM view_entry WHERE node = ?1")?;
        let rows = stmt.query_map([node], |r| {
            let dev: Option<i64> = r.get(2)?;
            let ino: Option<i64> = r.get(3)?;
            Ok((
                PathBuf::from(r.get::<_, String>(0)?),
                Owned {
                    source: PathBuf::from(r.get::<_, String>(1)?),
                    identity: dev.zip(ino).map(|(d, i)| (d as u64, i as u64)),
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn transaction(&mut self) -> Result<rusqlite::Transaction<'_>> {
        Ok(self.conn.transaction()?)
    }

    /// Replaces the emulator catalog tables with `file`. Reference data only: user choices live
    /// in the settings file, so nothing of the user's is lost.
    pub fn load_emulator_catalog(&mut self, file: &CatalogFile) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute_batch(
            "DELETE FROM emu_support; DELETE FROM emu_emulator; DELETE FROM emu_system;",
        )?;
        for s in &file.systems {
            tx.execute(
                "INSERT INTO emu_system (id, name) VALUES (?1, ?2)",
                params![s.id, s.name],
            )?;
        }
        for e in &file.emulators {
            tx.execute(
                "INSERT INTO emu_emulator (id, name, kind, corename) VALUES (?1, ?2, ?3, ?4)",
                params![e.id, e.name, e.kind, e.corename],
            )?;
            for s in &e.systems {
                tx.execute(
                    "INSERT OR IGNORE INTO emu_support (emulator, system) VALUES (?1, ?2)",
                    params![e.id, s],
                )?;
            }
        }
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('emulator_catalog', ?1)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [&file.generated],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// system id → full name, for every catalog system.
    pub fn system_names(&self) -> Result<HashMap<String, String>> {
        let mut stmt = self.conn.prepare("SELECT id, name FROM emu_system")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// system id → catalog emulators that run it, sorted by name.
    pub fn emulators_by_system(&self) -> Result<BTreeMap<String, Vec<EmulatorInfo>>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.system, e.id, e.name, e.kind, e.corename
             FROM emu_support s JOIN emu_emulator e ON e.id = s.emulator
             ORDER BY s.system, lower(e.name), e.kind",
        )?;
        let mut out: BTreeMap<String, Vec<EmulatorInfo>> = BTreeMap::new();
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                EmulatorInfo {
                    id: r.get(1)?,
                    name: r.get(2)?,
                    kind: Kind::from_catalog(&r.get::<_, String>(3)?),
                    corename: r.get(4)?,
                },
            ))
        })?;
        for row in rows {
            let (system, emu) = row?;
            out.entry(system).or_default().push(emu);
        }
        Ok(out)
    }
}

fn utf8(p: &Path) -> Result<&str> {
    p.to_str()
        .with_context(|| format!("non-UTF-8 path {}", p.display()))
}

/// Records a link RomServer made, with the identity (device, inode) of the file it links to.
pub fn record(
    tx: &rusqlite::Transaction,
    node: &str,
    path: &Path,
    source: &Path,
    identity: (u64, u64),
) -> Result<()> {
    tx.execute(
        "INSERT INTO view_entry (node, path, source, src_dev, src_ino) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (node, path) DO UPDATE SET
             source = excluded.source, src_dev = excluded.src_dev, src_ino = excluded.src_ino",
        params![
            node,
            utf8(path)?,
            utf8(source)?,
            identity.0 as i64,
            identity.1 as i64
        ],
    )?;
    Ok(())
}

pub fn forget(tx: &rusqlite::Transaction, node: &str, path: &Path) -> Result<()> {
    tx.execute(
        "DELETE FROM view_entry WHERE node = ?1 AND path = ?2",
        params![node, utf8(path)?],
    )?;
    Ok(())
}
