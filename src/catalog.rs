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

impl Catalog {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
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
             );",
        )?;
        Ok(Self { conn })
    }

    /// view path → library source, for everything this node's view owns.
    pub fn owned(&self, node: &str) -> Result<BTreeMap<PathBuf, PathBuf>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, source FROM view_entry WHERE node = ?1")?;
        let rows = stmt.query_map([node], |r| {
            Ok((
                PathBuf::from(r.get::<_, String>(0)?),
                PathBuf::from(r.get::<_, String>(1)?),
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

pub fn record(tx: &rusqlite::Transaction, node: &str, path: &Path, source: &Path) -> Result<()> {
    tx.execute(
        "INSERT INTO view_entry (node, path, source) VALUES (?1, ?2, ?3)
         ON CONFLICT (node, path) DO UPDATE SET source = excluded.source",
        params![node, utf8(path)?, utf8(source)?],
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
