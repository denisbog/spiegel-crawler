//! The JSON endpoints behind the UI — no browser needed.
//!
//! Found by capturing the DevTools network log of
//! `https://www.spiegel.de/fuermich/merkliste` (see `--dump-network`):
//!
//! ```text
//! GET https://www.spiegel.de/services/depot/api/v1/bookmarks
//!     -> ["019e15ed-…","f9b2ff76-…", …]                     (the bookmarked ids)
//! GET https://www.spiegel.de/services/sitesearch/fetch?ids=<csv>
//!     -> {"results":[{"url":…,"title":…,"access_level":…}]}  (the article data)
//! ```
//!
//! Both need nothing but the session cookies, so the Merkliste ("Ihre Artikel")
//! can be crawled without launching a browser at all.

use crate::fetch::Fetcher;
use anyhow::{Context, Result};
use serde_json::Value;

pub const BOOKMARKS_API: &str = "https://www.spiegel.de/services/depot/api/v1/bookmarks";
pub const SEARCH_API: &str = "https://www.spiegel.de/services/sitesearch/fetch";

#[derive(Debug, Clone)]
pub struct Bookmark {
    pub url: String,
    pub title: String,
    pub access_level: String,
}

/// The bookmarked article ids. An empty list means "logged in, nothing saved";
/// an error means the request itself failed (usually a missing session).
pub fn bookmark_ids(fetcher: &Fetcher) -> Result<Vec<String>> {
    let (_, body) = fetcher
        .text_with_accept(BOOKMARKS_API, "application/json")
        .with_context(|| format!("GET {BOOKMARKS_API}"))?;
    let v: Value = serde_json::from_str(&body)
        .with_context(|| format!("unexpected response from {BOOKMARKS_API}: {}", snippet(&body)))?;
    let ids: Vec<String> = match v {
        Value::Array(items) => items
            .iter()
            .filter_map(|i| match i {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o.get("id").and_then(Value::as_str).map(String::from),
                _ => None,
            })
            .collect(),
        Value::Object(ref o) => o
            .get("ids")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    Ok(ids)
}

/// Article data (url, title, access level) for a list of ids.
pub fn articles_by_ids(fetcher: &Fetcher, ids: &[String]) -> Result<Vec<Bookmark>> {
    let mut out = Vec::new();
    for chunk in ids.chunks(40) {
        let url = format!("{SEARCH_API}?ids={}", chunk.join(","));
        let (_, body) = fetcher
            .text_with_accept(&url, "application/json")
            .with_context(|| format!("GET {}", url))?;
        let v: Value = serde_json::from_str(&body)
            .with_context(|| format!("unexpected response from {SEARCH_API}: {}", snippet(&body)))?;
        let results = v
            .get("results")
            .and_then(Value::as_array)
            .or_else(|| v.as_array())
            .cloned()
            .unwrap_or_default();
        for r in results {
            let link = r
                .get("url")
                .and_then(Value::as_str)
                .map(String::from)
                .or_else(|| r.get("url_absolute").and_then(Value::as_str).map(String::from))
                .unwrap_or_default();
            if link.is_empty() {
                continue;
            }
            out.push(Bookmark {
                url: link,
                title: r
                    .get("title")
                    .or_else(|| r.get("heading"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                access_level: r
                    .get("access_level")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
    }
    Ok(out)
}

/// The whole "Ihre Artikel" list, browser-free.
pub fn bookmarks(fetcher: &Fetcher) -> Result<Vec<Bookmark>> {
    let ids = bookmark_ids(fetcher)?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    articles_by_ids(fetcher, &ids)
}

fn snippet(body: &str) -> String {
    body.chars().take(120).collect::<String>().replace('\n', " ")
}
