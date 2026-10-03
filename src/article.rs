//! Article parsing.
//!
//! spiegel.de article pages are server rendered, so no JS engine is needed.
//! Three sources are combined:
//!
//!  1. `<script type="application/ld+json">` (schema.org NewsArticle)
//!     -> headline, authors, dates, section, images, isAccessibleForFree
//!  2. `<script type="application/settings+json">`
//!     -> app.pageContext.clip.audioUrl / downloadUrl / duration  (the audio)
//!     -> app.pageContext.documentId, page.info.title
//!     -> paywall.attributes.is_active
//!  3. the rendered `<article>` element
//!     -> headings, paragraphs, quotes, lists, figures/images

use crate::util::{clean, date_prefix, safe_file_name};
use scraper::{ElementRef, Html, Selector};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::OnceLock;
use url::Url;

/// Containers we never want text from.
const SKIP_AREAS: &[&str] = &[
    "related_articles",
    "article-footer",
    "feature-bar",
    "header-bar",
    "nav-bar",
    "breaking-bar",
    "smartfeed",
    "block>smartfeed",
    "preferred-sources",
    "bookmark_button",
    "metered-unlocked-box",
    "title-cluster",
    "article-teaser",
    "article-footer>links",
];

/// Class-name fragments that mark ads / recommendations / paywall hints.
const SKIP_CLASSES: &[&str] = &[
    "advert",
    "outbrain",
    "taboola",
    "paywall",
    "newsletter",
    "related",
    "recommend",
];

/// `data-component` markers for boxes we never want (gift/paywall/ad boxes).
const SKIP_COMPONENTS: &[&str] = &["gift-box", "paywall", "advert", "recommend", "newsletter"];

/// Sentence openers that mark site furniture rather than article text.
const BOILERPLATE: &[&str] = &[
    "Dieser Artikel gehört zum Angebot von SPIEGEL+",
    "Sie haben bereits ein Abo",
    "Jetzt weiterlesen",
    "Zur Merkliste hinzufügen",
    "Artikel auf die Merkliste",
    "Melden Sie sich an",
    "Diesen Artikel teilen",
    "Folgen Sie uns",
    "Mehr zum Thema",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    pub id: String,
    pub url: String,
    pub width: Option<u32>,
    pub alt: String,
    pub caption: String,
    pub credit: String,
    /// relative path, filled in after download
    pub file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Audio {
    pub url: String,
    pub download_url: Option<String>,
    pub duration_ms: Option<u64>,
    pub duration_text: String,
    pub has_access: Option<bool>,
    pub kind: Option<String>,
    pub headline: String,
    pub file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Head { level: u8, text: String },
    Para { text: String },
    Quote { text: String, source: String },
    Item { text: String },
    Image { index: usize },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Article {
    pub url: String,
    pub canonical_url: String,
    pub id: String,
    pub title: String,
    pub kicker: String,
    pub description: String,
    pub section: String,
    pub authors: Vec<String>,
    pub published: String,
    pub modified: String,
    pub language: String,
    pub free: Option<bool>,
    pub paywalled: bool,
    pub tags: Vec<String>,
    pub word_count: usize,
    pub blocks: Vec<Block>,
    pub images: Vec<Image>,
    pub audio: Option<Audio>,
    pub fetched_at: String,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl Block {
    pub fn text_of(&self) -> Option<&str> {
        match self {
            Block::Head { text, .. } | Block::Para { text } | Block::Item { text } => Some(text),
            Block::Quote { text, .. } => Some(text),
            Block::Image { .. } => None,
        }
    }
}

impl Article {
    pub fn has_body(&self) -> bool {
        self.blocks.iter().any(|b| {
            matches!(b, Block::Para { text } if text.split_whitespace().count() > 3)
        })
    }
}

pub fn parse(url: &str, html: &str, fetched_at: &str) -> Article {
    let doc = Html::parse_document(html);
    let settings = script_json(&doc, r#"script[type="application/settings+json"]"#);
    let ld = script_json(&doc, r#"script[type="application/ld+json"]"#)
        .and_then(|v| find_article_node(&v).cloned());

    let mut warnings = Vec::new();
    if settings.is_none() {
        warnings.push("no application/settings+json script".into());
    }
    if ld.is_none() {
        warnings.push("no schema.org NewsArticle JSON-LD".into());
    }

    let title = json_str(ld.as_ref(), &["headline"])
        .or_else(|| json_path(settings.as_ref(), &["page", "info", "title"]).and_then(Value::as_str).map(String::from))
        .or_else(|| doc.select(&sel("h1")).next().map(|e| clean(&e.text().collect::<String>())))
        .unwrap_or_default();

    let kicker = json_path(settings.as_ref(), &["app", "pageContext", "clip", "kicker"])
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let description = json_str(ld.as_ref(), &["description"])
        .or_else(|| meta(&doc, "name", "description"))
        .unwrap_or_default();

    let section = json_str(ld.as_ref(), &["articleSection"]).unwrap_or_default();
    let authors = json_authors(ld.as_ref());
    let published = json_str(ld.as_ref(), &["datePublished"]).unwrap_or_default();
    let modified = json_str(ld.as_ref(), &["dateModified"]).unwrap_or_default();
    let free = ld.as_ref().and_then(|v| v.get("isAccessibleForFree")).and_then(Value::as_bool);
    let tags = match ld.as_ref().and_then(|v| v.get("keywords")) {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        Some(Value::String(s)) => s.split(',').map(|s| s.trim().to_string()).collect(),
        _ => Vec::new(),
    };
    let language = json_path(settings.as_ref(), &["page", "info", "language"])
        .and_then(Value::as_str)
        .unwrap_or("de")
        .to_string();
    let canonical_url = json_path(settings.as_ref(), &["page", "info", "canonical_url"])
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| meta(&doc, "property", "og:url"))
        .unwrap_or_else(|| url.to_string());
    let id = json_path(settings.as_ref(), &["app", "pageContext", "documentId"])
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| article_id_from_url(url))
        .unwrap_or_default();

    // ---- audio -----------------------------------------------------------
    let audio = extract_audio(settings.as_ref(), html, &title, &kicker);

    let (blocks, mut images, body_found) = extract_body(&doc, &title);
    if !body_found {
        warnings.push("no <article> element found (login wall / consent page?)".into());
    }
    // Lead/top image mostly lives outside the body walk: add it from schema.org/og.
    let mut seen_ids: HashSet<String> = images.iter().map(|i| i.id.clone()).collect();
    for url in extra_image_urls(ld.as_ref(), &doc) {
        let id = image_id(&url).unwrap_or_else(|| url.clone());
        if seen_ids.insert(id.clone()) {
            images.push(Image {
                id,
                width: width_from_url(&url),
                url,
                alt: title.clone(),
                caption: String::new(),
                credit: String::new(),
                file: None,
            });
        }
    }

    let paywall_active = json_path(settings.as_ref(), &["paywall", "attributes", "is_active"])
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let truncated = html.contains("Weiterlesen mit SPIEGEL+")
        || html.contains("Jetzt weiterlesen")
        || html.contains("Sie haben bereits ein Abo?");
    // With a live SPIEGEL+ session the text is complete even though the static
    // metadata says `isAccessibleForFree: false`.  Only positive evidence counts:
    // the server's own paywall state, or the truncation markers.
    let paywalled = paywall_active || truncated;
    if paywalled {
        warnings.push(format!(
            "content looks truncated (paywall is_active={paywall_active}, markers={truncated}, isAccessibleForFree={free:?})"
        ));
    }

    let word_count = blocks
            .iter()
            .filter_map(|b| b.text_of())
            .map(|t| t.split_whitespace().count())
            .sum::<usize>();

    Article {
        url: url.to_string(),
        canonical_url,
        id,
        title,
        kicker,
        description,
        section,
        authors,
        published,
        modified,
        language,
        free,
        paywalled,
        tags,
        word_count,
        blocks,
        images,
        audio,
        fetched_at: fetched_at.to_string(),
        warnings,
    }
}

/// `<folder>/<date>_<slug>` for this article.
pub fn dir_name(a: &Article) -> String {
    let date = date_prefix(&a.published);
    let slug = if a.title.is_empty() {
        a.id.clone()
    } else {
        crate::util::slugify(&a.title)
    };
    let slug = if slug.is_empty() { "article".to_string() } else { slug };
    format!("{date}_{slug}")
}

fn extract_audio(settings: Option<&Value>, html: &str, title: &str, kicker: &str) -> Option<Audio> {
    if let Some(clip) = json_path(settings, &["app", "pageContext", "clip"]) {
        if let Some(url) = clip.get("audioUrl").and_then(Value::as_str) {
            let duration_ms = clip.get("duration").and_then(Value::as_u64);
            return Some(Audio {
                url: url.to_string(),
                download_url: clip.get("downloadUrl").and_then(Value::as_str).map(String::from),
                duration_ms,
                duration_text: duration_ms
                    .map(|ms| format!("{} Min", (ms as f64 / 60000.0).round().max(1.0) as u64))
                    .unwrap_or_default(),
                has_access: clip.get("hasAccess").and_then(Value::as_bool),
                kind: clip.get("kicker").and_then(Value::as_str).map(String::from),
                headline: clip
                    .get("headline")
                    .and_then(Value::as_str)
                    .unwrap_or(title)
                    .to_string(),
                file: None,
            });
        }
    }
    (|| {
        // Fallback for pages without the settings blob.
        let mut cands: Vec<String> = Vec::new();
        let doc = Html::parse_document(html);
        if let Some(src) = doc.select(&sel("audio")).next().and_then(|e| e.value().attr("src")) {
            cands.push(src.to_string());
        }
        for e in doc.select(&sel("audio source")) {
            if let Some(src) = e.value().attr("src") {
                cands.push(src.to_string());
            }
        }
        let unescaped = html.replace("\\u0026", "&").replace("\\/", "/");
        for m in mp3_regex().find_iter(&unescaped) {
            cands.push(m.as_str().to_string());
        }
        let url = cands.into_iter().find(|u| u.starts_with("http"))?;
        Some(Audio {
            url: url.clone(),
            download_url: None,
            duration_ms: None,
            duration_text: String::new(),
            has_access: None,
            kind: Some("unknown".into()),
            headline: if title.is_empty() { kicker.to_string() } else { title.to_string() },
            file: None,
        })
    })()
}

/// Walk the rendered `<article>` in document order.
fn extract_body(doc: &Html, title: &str) -> (Vec<Block>, Vec<Image>, bool) {
    let title_key = normalize_key(title);
    let mut blocks = Vec::new();
    let mut images: Vec<Image> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let Some(article) = doc.select(&sel("article")).next() else {
        return (blocks, images, false);
    };

    for el in article.select(&sel("h1, h2, h3, h4, p, blockquote, li, figure, img, section")) {
        if ancestors_skipped(el) {
            continue;
        }
        match el.value().name() {
            "section" => {
                // Pull quote: big text plus attribution in .RichTextCaption
                if el.value().attr("data-area") == Some("quote") {
                    let source = el
                        .select(&sel(".RichTextCaption"))
                        .next()
                        .map(|c| clean(&c.text().collect::<String>()))
                        .unwrap_or_default();
                    let full = clean(&el.text().collect::<String>());
                    let text = clean(&full.replace(&source, ""));
                    if !text.is_empty() {
                        blocks.push(Block::Quote { text, source });
                    }
                }
            }
            "figure" => {
                let mut added = false;
                for img in el.select(&sel("img")) {
                    if let Some(idx) = push_image(&mut images, &mut seen, &img, el) {
                        blocks.push(Block::Image { index: idx });
                        added = true;
                    }
                }
                if !added {
                    // figure with only text (quote box etc.) -> keep the words
                    let t = clean(&el.text().collect::<String>());
                    if !t.is_empty() && !is_boilerplate(&t) {
                        blocks.push(Block::Para { text: t });
                    }
                }
            }
            "img" => {
                if inside_figure(el) {
                    continue;
                }
                if let Some(idx) = push_image(&mut images, &mut seen, &el, el) {
                    blocks.push(Block::Image { index: idx });
                }
            }
            tag => {
                let text = clean(&el.text().collect::<String>());
                if text.is_empty() || is_boilerplate(&text) {
                    continue;
                }
                match tag {
                    "h1" | "h2" => {
                        // the page repeats the headline as an in-article <h1>
                        let key = normalize_key(&text);
                        if title_key.is_empty() || key.is_empty() || !title_key.starts_with(&key) {
                            blocks.push(Block::Head { level: 2, text });
                        }
                    }
                    "h3" | "h4" => blocks.push(Block::Head { level: 3, text }),
                    "blockquote" => {
                        let (text, source) = split_quote(&text);
                        blocks.push(Block::Quote { text, source });
                    }
                    "li" => {
                        // Skip navigation-ish lists that survived the area filter.
                        if text.split_whitespace().count() <= 1 && text.len() < 4 {
                            continue;
                        }
                        blocks.push(Block::Item { text });
                    }
                    "p" => blocks.push(Block::Para { text }),
                    _ => {}
                }
            }
        }
    }

    (blocks, images, true)
}

fn push_image(
    images: &mut Vec<Image>,
    seen: &mut HashSet<String>,
    img: &ElementRef,
    scope: ElementRef,
) -> Option<usize> {
    let (url, width) = best_image_url(img)?;
    let id = image_id(&url).unwrap_or_else(|| url.clone());
    if !seen.insert(id.clone()) {
        return None;
    }
    let caption = caption_of(scope);
    let (caption, credit) = split_credit(&caption);
    images.push(Image {
        id,
        url,
        width,
        alt: img.value().attr("alt").unwrap_or("").trim().to_string(),
        caption,
        credit,
        file: None,
    });
    Some(images.len() - 1)
}

/// Pick the widest candidate from `srcset` (jpg beats webp on a tie).
fn best_image_url(img: &ElementRef) -> Option<(String, Option<u32>)> {
    let mut cands: Vec<(String, Option<u32>)> = Vec::new();
    for attr in ["srcset", "data-srcset"] {
        if let Some(v) = img.value().attr(attr) {
            cands.extend(parse_srcset(v));
        }
    }
    // <picture><source srcset="..."> holds the real (webp) candidates
    if let Some(pic) = img.parent().and_then(ElementRef::wrap) {
        if pic.value().name() == "picture" {
            for s in pic.select(&sel("source")) {
                for attr in ["srcset", "data-srcset"] {
                    if let Some(v) = s.value().attr(attr) {
                        cands.extend(parse_srcset(v));
                    }
                }
            }
        }
    }
    for attr in ["src", "data-src"] {
        if let Some(v) = img.value().attr(attr) {
            if v.starts_with("http") {
                cands.push((v.to_string(), width_from_url(v)));
            }
        }
    }
    cands.retain(|(u, _)| u.starts_with("http") && !u.starts_with("data:"));
    cands.sort_by(|a, b| {
        let w = b.1.unwrap_or(0).cmp(&a.1.unwrap_or(0));
        if w != std::cmp::Ordering::Equal {
            return w;
        }
        let jpg = |u: &String| u.ends_with(".jpg") || u.ends_with(".jpeg");
        jpg(&b.0).cmp(&jpg(&a.0))
    });
    cands.into_iter().next()
}

fn parse_srcset(v: &str) -> Vec<(String, Option<u32>)> {
    srcset_regex()
        .captures_iter(v)
        .filter_map(|c| {
            let url = c.name("url")?.as_str().to_string();
            if url.starts_with("data:") {
                return None;
            }
            let width = c
                .name("desc")
                .and_then(|d| d.as_str().strip_suffix('w'))
                .and_then(|n| n.trim().parse::<u32>().ok())
                .or_else(|| width_from_url(&url));
            Some((url, width))
        })
        .collect()
}

/// spiegel image urls embed the requested width: `..._w960_r1.5_fpx.._fpy...jpg`
fn width_from_url(u: &str) -> Option<u32> {
    let caps = image_regex().captures(u)?;
    caps.name("w")?.as_str().parse().ok()
}

fn image_id(u: &str) -> Option<String> {
    image_regex()
        .captures(u)
        .and_then(|c| c.name("id").map(|m| m.as_str().to_string()))
}

fn caption_of(scope: ElementRef) -> String {
    // figure > figcaption, or the element's own parent figcaption
    if let Some(c) = scope.select(&sel("figcaption")).next() {
        return clean(&c.text().collect::<String>());
    }
    if let Some(p) = scope.parent().and_then(ElementRef::wrap) {
        if p.value().name() == "figcaption" {
            return clean(&p.text().collect::<String>());
        }
        if let Some(c) = p.select(&sel("figcaption")).next() {
            return clean(&c.text().collect::<String>());
        }
    }
    scope.value().attr("title").map(clean).unwrap_or_default()
}

fn split_credit(caption: &str) -> (String, String) {
    if let Some(c) = credit_regex().captures(caption) {
        let whole = c.get(0).map(|m| m.as_str()).unwrap_or("");
        let credit = c.name("credit").map(|m| clean(m.as_str())).unwrap_or_default();
        let text = clean(&caption.replace(whole, ""));
        return (text, credit);
    }
    (caption.to_string(), String::new())
}

fn split_quote(text: &str) -> (String, String) {
    for sep in ["«, sagte", ", sagte", "» sagte"] {
        if let Some(i) = text.find(sep) {
            let (q, rest) = text.split_at(i);
            return (clean(q.trim_end_matches([',', ' '])), clean(rest));
        }
    }
    (text.to_string(), String::new())
}

fn inside_figure(el: ElementRef) -> bool {
    let mut cur = el.parent();
    while let Some(node) = cur {
        if let Some(e) = ElementRef::wrap(node) {
            if e.value().name() == "figure" {
                return true;
            }
        }
        cur = node.parent();
    }
    false
}

fn ancestors_skipped(el: ElementRef) -> bool {
    let mut cur = el.parent();
    while let Some(node) = cur {
        if let Some(e) = ElementRef::wrap(node) {
            let v = e.value();
            if let Some(area) = v.attr("data-area") {
                // pull quotes are emitted from their <section>, so skip the parts
                if SKIP_AREAS.contains(&area) || area == "quote" {
                    return true;
                }
            }
            if let Some(comp) = v.attr("data-component") {
                let comp = comp.to_ascii_lowercase();
                if SKIP_COMPONENTS.iter().any(|c| comp.contains(c)) {
                    return true;
                }
            }
            if v.attr("role") == Some("navigation") {
                return true;
            }
            let cls = v.attr("class").unwrap_or("").to_ascii_lowercase();
            if SKIP_CLASSES.iter().any(|c| cls.contains(c)) || cls.contains("richtextcaption") {
                return true;
            }
            if matches!(v.name(), "nav" | "footer" | "aside" | "figcaption") {
                return true;
            }
        }
        cur = node.parent();
    }
    false
}

// ---------------------------------------------------------------- helpers

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("valid selector")
}

/// Lowercase alphanumeric only – used to compare headings with the title.
fn normalize_key(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

fn is_boilerplate(t: &str) -> bool {
    BOILERPLATE.iter().any(|b| t.contains(b))
}

/// Lead images: schema.org `image`, `thumbnailUrl` and og:image.
fn extra_image_urls(ld: Option<&Value>, doc: &Html) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    fn collect(out: &mut Vec<String>, v: &Value) {
        match v {
            Value::String(s) if s.starts_with("http") => out.push(s.clone()),
            Value::Object(o) => {
                for k in ["url", "contentUrl"] {
                    if let Some(u) = o.get(k).and_then(Value::as_str) {
                        if u.starts_with("http") {
                            out.push(u.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(ld) = ld {
        for key in ["image", "thumbnailUrl"] {
            match ld.get(key) {
                Some(Value::Array(a)) => a.iter().for_each(|v| collect(&mut out, v)),
                Some(other) => collect(&mut out, other),
                None => {}
            }
        }
    }
    if let Some(og) = meta(doc, "property", "og:image") {
        out.push(og);
    }
    // prefer the widest variant for a given image id
    out.sort_by_key(|u| std::cmp::Reverse(width_from_url(u).unwrap_or(0)));
    out
}

/// Article links found in raw HTML (used for listing pages).
/// Prefers the `<main>` element: headers/footers are full of unrelated articles.
pub fn links_in_html(html: &str) -> Vec<String> {
    links_in(main_slice(html).unwrap_or(html))
}

fn links_in(html: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in article_url_regex().find_iter(html) {
        out.push(c.as_str().to_string());
    }
    for c in relative_article_href_regex().captures_iter(html) {
        if let Some(m) = c.get(1) {
            out.push(format!("https://www.spiegel.de{}", m.as_str()));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// `<main>…</main>` if the page has one (nav/footer excluded).
fn main_slice(html: &str) -> Option<&str> {
    let start = html.find("<main")?;
    let open_end = html[start..].find('>')? + start;
    let end = html[open_end..].find("</main>")? + open_end;
    Some(&html[open_end..end])
}

fn script_json(doc: &Html, s: &str) -> Option<Value> {
    let el = doc.select(&sel(s)).next()?;
    let raw: String = el.text().collect();
    serde_json::from_str(&raw).ok()
}

fn meta(doc: &Html, key: &str, value: &str) -> Option<String> {
    let s = Selector::parse(&format!(r#"meta[{key}="{value}"]"#)).ok()?;
    doc.select(&s).next()?.value().attr("content").map(clean)
}

fn find_article_node(v: &Value) -> Option<&Value> {
    match v {
        Value::Array(a) => a.iter().find_map(find_article_node),
        Value::Object(m) => {
            if let Some(t) = m.get("@type") {
                let hit = match t {
                    Value::String(s) => s.contains("Article"),
                    Value::Array(a) => a.iter().any(|x| x.as_str().is_some_and(|s| s.contains("Article"))),
                    _ => false,
                };
                if hit {
                    return Some(v);
                }
            }
            m.get("@graph").and_then(find_article_node)
        }
        _ => None,
    }
}

fn json_path<'a>(v: Option<&'a Value>, path: &[&str]) -> Option<&'a Value> {
    let mut cur = v?;
    for p in path {
        cur = cur.get(*p)?;
    }
    Some(cur)
}

fn json_str(v: Option<&Value>, path: &[&str]) -> Option<String> {
    json_path(v, path).and_then(Value::as_str).map(|s| clean(s))
}

fn json_authors(ld: Option<&Value>) -> Vec<String> {
    let Some(a) = ld.and_then(|v| v.get("author")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut push = |v: &Value| match v {
        Value::String(s) => out.push(clean(s)),
        Value::Object(o) => {
            if let Some(n) = o.get("name").and_then(Value::as_str) {
                out.push(clean(n));
            }
        }
        _ => {}
    };
    match a {
        Value::Array(arr) => arr.iter().for_each(push),
        other => push(other),
    }
    out.retain(|s| !s.is_empty());
    out
}

fn article_id_from_url(u: &str) -> Option<String> {
    let id_re = Regex::new(r"-a-([0-9a-fA-F-]{36})").ok()?;
    id_re.captures(u)?.get(1).map(|m| m.as_str().to_string())
}

/// `-a-` id regex, compiled once.
pub fn article_url_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"https?://www\.spiegel\.de/[^\s"'<>\\)]*-a-[0-9a-fA-F]{8}-[0-9a-fA-F-]{27}"#).unwrap()
    })
}

pub fn relative_article_href_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"href="(/[^"]*-a-[0-9a-fA-F]{8}-[0-9a-fA-F-]{27})""#).unwrap())
}

fn mp3_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"https?://[^\s"'\\<>]+\.mp3[^\s"'\\<>]*"#).unwrap())
}

fn srcset_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // data: URIs contain commas, so parse with a regex rather than splitting on ','.
    RE.get_or_init(|| {
        Regex::new(r"(?P<url>https?://[^\s,]+)(?:\s+(?P<desc>\d+w|[\d.]+x))?").unwrap()
    })
}

fn image_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"images/(?P<id>[0-9a-fA-F-]{36})(?:_w(?P<w>\d+))?").unwrap()
    })
}

fn credit_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:foto|quelle|credit|bild(?:nachweis)?)\b\s*:?\s*(?P<credit>.+)$").unwrap()
    })
}


/// Strip the fragment and tracking params from a discovered URL.
pub fn normalize_url(raw: &str) -> Option<String> {
    let raw = raw.trim().trim_end_matches(['.', ',', ')', '"', '\'', '>']);
    let mut u = Url::parse(raw).ok()?;
    u.set_fragment(None);
    if u.host_str() != Some("www.spiegel.de") {
        return None;
    }
    Some(u.to_string())
}

/// Name of the downloaded image file, e.g. `03_5ecc597d-..._w1040.jpg`.
pub fn image_file_name(index: usize, img: &Image) -> String {
    let ext = Url::parse(&img.url)
        .ok()
        .and_then(|u| {
            u.path_segments()?
                .last()
                .and_then(|s| s.rsplit_once('.'))
                .map(|(_, e)| e.to_ascii_lowercase())
        })
        .filter(|e| e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| "jpg".to_string());
    let w = img.width.map(|w| format!("_w{w}")).unwrap_or_default();
    format!("{:02}_{}{}.{}", index + 1, safe_file_name(&img.id), w, ext)
}
