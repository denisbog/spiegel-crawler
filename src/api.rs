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
use anyhow::{bail, Context, Result};
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
    let mut unique: Vec<String> = Vec::with_capacity(ids.len());
    for id in ids {
        if !unique.contains(&id) {
            unique.push(id);
        }
    }
    Ok(unique)
}

/// Article data (url, title, access level) for a list of ids.
/// Also returns the ids that came back without a usable url.
pub fn articles_by_ids(fetcher: &Fetcher, ids: &[String]) -> Result<(Vec<Bookmark>, Vec<String>)> {
    let mut out = Vec::new();
    let mut unusable = Vec::new();
    let mut seen: Vec<String> = Vec::new();
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
            let id = r.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
            let link = r
                .get("url")
                .and_then(Value::as_str)
                .map(String::from)
                .or_else(|| r.get("url_absolute").and_then(Value::as_str).map(String::from))
                .unwrap_or_default();
            if link.is_empty() {
                unusable.push(if id.is_empty() { "<no id>".to_string() } else { id });
                continue;
            }
            seen.push(id);
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
    let mut unique_items: Vec<Bookmark> = Vec::with_capacity(out.len());
    for b in out {
        if !unique_items.iter().any(|x| x.url == b.url) {
            unique_items.push(b);
        }
    }
    let out = unique_items;

    // ids the search endpoint never answered for
    for id in ids {
        if !seen.contains(id) && !unusable.contains(id) {
            unusable.push(id.clone());
        }
    }
    Ok((out, unusable))
}

pub struct Bookmarks {
    pub items: Vec<Bookmark>,
    pub requested: usize,
}

/// The whole "Ihre Artikel" list, browser-free.
///
/// Strict by default: a list that comes back shorter than the ids we asked for
/// means part of your Merkliste would be silently missing, so it is an error
/// unless `allow_partial` is set.
pub fn bookmarks(fetcher: &Fetcher, allow_partial: bool) -> Result<Bookmarks> {
    let ids = bookmark_ids(fetcher)?;
    if ids.is_empty() {
        return Ok(Bookmarks { items: Vec::new(), requested: 0 });
    }
    let requested = ids.len();
    let (items, unusable) = articles_by_ids(fetcher, &ids)?;
    check_complete(requested, items.len(), &unusable, allow_partial)?;
    Ok(Bookmarks { items, requested })
}

/// Shared by `bookmarks` and the unit tests: is this list complete?
fn check_complete(requested: usize, got: usize, unusable: &[String], allow_partial: bool) -> Result<()> {
    if got == requested && unusable.is_empty() {
        return Ok(());
    }
    let detail = if unusable.is_empty() {
        String::new()
    } else {
        let mut shown = unusable.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
        if unusable.len() > 5 {
            shown.push_str(&format!(", … (+{} more)", unusable.len() - 5));
        }
        format!("; no usable entry for: {shown}")
    };
    let msg = format!(
        "bookmarks API returned {got} usable entries for {requested} bookmarked ids{detail}"
    );
    if allow_partial {
        eprintln!("  ! {msg} – crawling the partial list (--allow-partial)");
        Ok(())
    } else {
        bail!("{msg}. Nothing was crawled; rerun with --allow-partial to accept the short list.")
    }
}

fn snippet(body: &str) -> String {
    body.chars().take(120).collect::<String>().replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_list_passes() {
        assert!(check_complete(5, 5, &[], false).is_ok());
    }

    #[test]
    fn short_list_fails_unless_allowed() {
        let err = check_complete(5, 3, &[], false).unwrap_err().to_string();
        assert!(err.contains("3 usable entries for 5"), "{err}");
        assert!(err.contains("allow-partial"), "{err}");
        assert!(check_complete(5, 3, &[], true).is_ok());
    }

    #[test]
    fn entries_without_a_url_count_as_missing() {
        let err = check_complete(3, 3, &["abc".to_string()], false).unwrap_err().to_string();
        assert!(err.contains("no usable entry for: abc"), "{err}");
    }
}
