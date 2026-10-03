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

}


/// Cookies the server only sets for a logged-in session
/// (`accessInfo` carries the entitlements, e.g. SPIEGEL+).
pub const SESSION_COOKIE_HINTS: &[&str] =
    &["accessInfo", "sara_user_session", "sara_user_session-id", "userInfo", "authId"];

/// Does this cookie set look like a live login?
pub fn looks_logged_in(cookies: &[StorageCookie]) -> bool {
    cookies.iter().any(|c| SESSION_COOKIE_HINTS.contains(&c.name.as_str()))
}

/// Put one stored cookie into a jar.
pub fn add_cookie(jar: &Jar, c: &StorageCookie) {
    let domain = c.domain.trim_start_matches('.').to_string();
    let domain = if domain.is_empty() { "www.spiegel.de".to_string() } else { domain };
    let path = if c.path.is_empty() { "/" } else { &c.path };
    let url = Url::parse(&format!("https://{domain}{path}"))
        .unwrap_or_else(|_| Url::parse("https://www.spiegel.de/").unwrap());
    let mut s = format!("{}={}; Path={path}", c.name, c.value);
    if !c.domain.is_empty() {
        s.push_str(&format!("; Domain={}", c.domain.trim_start_matches('.')));
    }
    if c.secure {
        s.push_str("; Secure");
    }
    if c.http_only {
        s.push_str("; HttpOnly");
    }
    jar.add_cookie_str(&s, &url);
}

/// `--cookie name=value`, valid for .spiegel.de.
pub fn add_raw_cookie(jar: &Jar, name: &str, value: &str) {
    if let Ok(url) = Url::parse("https://www.spiegel.de/") {
        jar.add_cookie_str(&format!("{name}={value}; Path=/; Domain=.spiegel.de"), &url);
    }
}

/// Parse one `Set-Cookie` header into a storable cookie.
pub fn parse_set_cookie(raw: &str, host: &str) -> Option<StorageCookie> {
    let mut parts = raw.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let mut c = StorageCookie {
        name: name.trim().to_string(),
        value: value.trim().to_string(),
        domain: host.to_string(),
        path: "/".to_string(),
        secure: false,
        http_only: false,
    };
    for attr in parts {
        let attr = attr.trim();
        let (k, v) = attr.split_once('=').map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .unwrap_or((attr.to_ascii_lowercase(), String::new()));
        match k.as_str() {
            "domain" => c.domain = v,
            "path" => c.path = v,
            "secure" => c.secure = true,
            "httponly" => c.http_only = true,
            _ => {}
        }
    }
    Some(c)
}

