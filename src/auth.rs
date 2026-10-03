//! Turn a Playwright `storage_state.json` into a reqwest cookie jar.
//!
//! The paywalled parts of spiegel.de (and the personal "Merkliste") need a
//! logged-in session.  Logging in from pure Rust would mean replaying the
//! SSO/JS login flow, so instead we let Playwright log in once and export the
//! cookie jar:
//!
//! ```python
//! context.storage_state(path="state.json")
//! ```
//!
//! `tools/dump_merkliste.py` does exactly that, plus dumps the article URLs.

use anyhow::{Context, Result};
use reqwest::cookie::Jar;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use url::Url;

#[derive(Debug, Serialize, Deserialize)]
pub struct StorageState {
    #[serde(default)]
    pub cookies: Vec<StorageCookie>,
    #[serde(default)]
    pub origins: Vec<Origin>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageCookie {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default = "root")]
    pub path: String,
    #[serde(default)]
    pub secure: bool,
    #[serde(default, alias = "httpOnly", alias = "http_only")]
    pub http_only: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Origin {
    pub origin: String,
    #[serde(default)]
    pub local_storage: Vec<LocalStorageItem>,
}

#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)] // kept for completeness; only cookies are replayed
pub struct LocalStorageItem {
    pub name: String,
    pub value: String,
}

fn root() -> String {
    "/".to_string()
}

impl StorageState {
    /// Wrap cookies obtained from the browser (see `browser.rs`).
    pub fn from_cookies(cookies: Vec<StorageCookie>) -> Self {
        Self { cookies, origins: Vec::new() }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("invalid JSON in {}", path.display()))
    }

    /// Build a cookie jar.  Playwright stores cookies for many domains; we keep
    /// the ones relevant for spiegel.de.
    pub fn into_jar(self) -> Result<Arc<Jar>> {
        let jar = Arc::new(Jar::default());
        let mut kept = 0usize;
        for c in &self.cookies {
            let domain = c.domain.trim_start_matches('.').to_string();
            let domain = if domain.is_empty() { "www.spiegel.de".to_string() } else { domain };
            let url = Url::parse(&format!("https://{}{}", domain, c.path))
                .unwrap_or_else(|_| Url::parse("https://www.spiegel.de/").unwrap());
            let mut s = format!("{}={}; Path={}", c.name, c.value, c.path);
            s.push_str(&format!("; Domain={}", c.domain.trim_start_matches('.')));
            if c.secure {
                s.push_str("; Secure");
            }
            if c.http_only {
                s.push_str("; HttpOnly");
            }
            jar.add_cookie_str(&s, &url);
            kept += 1;
        }
        let ls: usize = self.origins.iter().map(|o| o.local_storage.len()).sum();
        if ls > 0 {
            eprintln!(
                "note: state.json also carries {ls} localStorage entries (origins: {}); \
                 this crawler only replays cookies",
                self.origins.iter().map(|o| o.origin.as_str()).collect::<Vec<_>>().join(", ")
            );
        }
        eprintln!("auth: loaded {kept} cookies from storage state");
        Ok(jar)
    }
}


/// Cookies the server only sets for a logged-in session
/// (`accessInfo` carries the entitlements, e.g. SPIEGEL+).
pub const SESSION_COOKIE_HINTS: &[&str] =
    &["accessInfo", "sara_user_session", "sara_user_session-id", "userInfo", "authId"];

/// Does this cookie set look like a live login?
pub fn looks_logged_in(cookies: &[StorageCookie]) -> bool {
    cookies.iter().any(|c| SESSION_COOKIE_HINTS.contains(&c.name.as_str()))
}

/// Build a jar from `--cookie name=value` pairs (all for .spiegel.de).
pub fn jar_from_pairs(pairs: &[String]) -> Result<Arc<Jar>> {
    let jar = Arc::new(Jar::default());
    let url = Url::parse("https://www.spiegel.de/")?;
    for p in pairs {
        let Some((name, value)) = p.split_once('=') else {
            anyhow::bail!("--cookie wants name=value, got {p:?}");
        };
        jar.add_cookie_str(&format!("{}={}; Path=/; Domain=.spiegel.de", name.trim(), value), &url);
    }
    Ok(jar)
}
