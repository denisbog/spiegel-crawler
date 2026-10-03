//! Import the session cookies of a locally installed browser.
//!
//! The cheapest honest way to get an authenticated session without automating a
//! login: if you are already logged in to spiegel.de in your own browser, take
//! those cookies.  Firefox stores them in plain text in `cookies.sqlite`
//! (Chrome encrypts them, so only Firefox is supported).
//!
//! Only `.spiegel.de` cookies are read, and only into `cookies.json`.

use crate::auth::StorageCookie;
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Firefox profiles, most recently touched cookie database first.
pub fn firefox_cookie_dbs() -> Vec<PathBuf> {
    let mut out: Vec<(SystemTime, PathBuf)> = Vec::new();
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    for root in [home.join(".mozilla/firefox"), home.join("snap/firefox/common/.mozilla/firefox")] {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in entries.flatten() {
            let db = e.path().join("cookies.sqlite");
            if let Ok(meta) = std::fs::metadata(&db) {
                let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
                out.push((mtime, db));
            }
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().map(|(_, p)| p).collect()
}

/// Read the non-expired spiegel.de cookies from a Firefox cookie database.
///
/// A running Firefox keeps a lock on the file, so we work on a copy
/// (`cookies.sqlite` + its `-wal`/`-shm` siblings).
pub fn read_spiegel_cookies(db: &Path) -> Result<Vec<StorageCookie>> {
    let tmp = std::env::temp_dir().join(format!("spiegel-crawler-cookies-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let copy = tmp.join("cookies.sqlite");
    std::fs::copy(db, &copy).with_context(|| format!("copying {}", db.display()))?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let src = PathBuf::from(format!("{}{suffix}", db.display()));
        if src.is_file() {
            let _ = std::fs::copy(&src, tmp.join(format!("cookies.sqlite{suffix}")));
        }
    }
    let cookies = read_db(&copy);
    let _ = std::fs::remove_dir_all(&tmp);
    cookies
}

fn read_db(db: &Path) -> Result<Vec<StorageCookie>> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", db.display()))?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let mut stmt = conn
        .prepare(
            "SELECT name, value, host, path, isSecure, isHttpOnly, expiry
               FROM moz_cookies
              WHERE host LIKE '%spiegel.de'
                AND (expiry = 0 OR expiry > ?1)",
        )
        .context("querying moz_cookies")?;
    let rows = stmt.query_map([now], |r| {
        Ok(StorageCookie {
            name: r.get(0)?,
            value: r.get(1)?,
            domain: r.get(2)?,
            path: r.get(3)?,
            secure: r.get::<_, i64>(4)? != 0,
            http_only: r.get::<_, i64>(5)? != 0,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Union of the spiegel cookies of every Firefox profile (later ones win).
pub fn import_firefox() -> Result<Vec<StorageCookie>> {
    let dbs = firefox_cookie_dbs();
    if dbs.is_empty() {
        anyhow::bail!("no Firefox profile with a cookies.sqlite found");
    }
    let mut out: Vec<StorageCookie> = Vec::new();
    for db in &dbs {
        match read_spiegel_cookies(db) {
            Ok(cs) if !cs.is_empty() => {
                eprintln!("firefox: {} cookies from {}", cs.len(), db.display());
                for c in cs {
                    if let Some(slot) = out.iter_mut().find(|e| e.name == c.name && e.domain == c.domain) {
                        *slot = c;
                    } else {
                        out.push(c);
                    }
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("firefox: skipping {}: {e:#}", db.display()),
        }
    }
    if out.is_empty() {
        anyhow::bail!("no spiegel.de cookies in any Firefox profile – log in there first");
    }
    Ok(out)
}
