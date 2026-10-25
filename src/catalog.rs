//! SQLite state. For now: which view paths RomServer created, so it never deletes anything
//! it didn't make.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

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
