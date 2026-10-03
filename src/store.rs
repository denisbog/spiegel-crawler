//! Writing results to disk: one folder per article with `article.md`,
//! `article.json`, `images/` and `audio/`.

use crate::article::{self, Article, Block};
use crate::fetch::Fetcher;
use crate::util::slugify;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub struct Store {
    pub root: PathBuf,
    pub images: bool,
    pub audio: bool,
    pub force: bool,
}

impl Store {
    pub fn new(root: &Path, images: bool, audio: bool, force: bool) -> Self {
        Self { root: root.to_path_buf(), images, audio, force }
    }

    pub fn dir_for(&self, a: &Article) -> PathBuf {
        self.root.join(article::dir_name(a))
    }

    /// Download media, then write markdown + json. Re-running is cheap:
    /// existing non-empty media files are skipped.
    pub fn save(&self, f: &Fetcher, a: &mut Article) -> Result<PathBuf> {
        let dir = self.dir_for(a);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut problems: Vec<String> = Vec::new();

        if self.images {
            for i in 0..a.images.len() {
                let name = article::image_file_name(i, &a.images[i]);
                let dest = dir.join("images").join(&name);
                let url = a.images[i].url.clone();
                match f.download(&url, &dest, self.force) {
                    Ok(n) => {
                        a.images[i].file = Some(format!("images/{name}"));
                        if f.verbose {
                            eprintln!("    image {name} ({} bytes)", if n == 0 { 0 } else { n });
                        }
                    }
                    Err(e) => problems.push(format!("image {url}: {e:#}")),
                }
            }
        }

        if self.audio {
            if let Some(au) = a.audio.as_mut() {
                let stem = if a.title.is_empty() { a.id.clone() } else { slugify(&a.title) };
                let stem = if stem.is_empty() { "audio".to_string() } else { stem };
                let name = format!("{stem}.mp3");
                let dest = dir.join("audio").join(&name);
                let url = au.download_url.clone().unwrap_or_else(|| au.url.clone());
                match f.download(&url, &dest, self.force) {
                    Ok(n) => {
                        au.file = Some(format!("audio/{name}"));
                        if f.verbose {
                            eprintln!("    audio {name} ({n} bytes)");
                        }
                    }
                    Err(e) => problems.push(format!("audio {url}: {e:#}")),
                }
            }
        }

        a.warnings.extend(problems);

        let md_path = dir.join("article.md");
        std::fs::write(&md_path, to_markdown(a)).with_context(|| format!("writing {}", md_path.display()))?;
        let json_path = dir.join("article.json");
        let json = serde_json::to_string_pretty(a)?;
        std::fs::write(&json_path, json).with_context(|| format!("writing {}", json_path.display()))?;
        Ok(dir)
    }
}

pub fn to_markdown(a: &Article) -> String {
    let mut s = String::with_capacity(4096);
    // YAML front matter: handy for later indexing.
    s.push_str("---\n");
    s.push_str(&format!("title: \"{}\"\n", yaml_escape(&a.title)));
    if !a.description.is_empty() {
        s.push_str(&format!("description: \"{}\"\n", yaml_escape(&a.description)));
    }
    s.push_str(&format!("url: {}\n", a.canonical_url));
    s.push_str(&format!("source_url: {}\n", a.url));
    s.push_str(&format!("id: {}\n", a.id));
    if !a.section.is_empty() {
        s.push_str(&format!("section: \"{}\"\n", yaml_escape(&a.section)));
    }
    s.push_str(&format!("published: \"{}\"\n", a.published));
    if !a.modified.is_empty() {
        s.push_str(&format!("modified: \"{}\"\n", a.modified));
    }
    if !a.authors.is_empty() {
        s.push_str(&format!("authors: [{}]\n", a.authors.iter().map(|x| format!("\"{}\"", yaml_escape(x))).collect::<Vec<_>>().join(", ")));
    }
    if !a.tags.is_empty() {
        s.push_str(&format!("tags: [{}]\n", a.tags.iter().map(|x| format!("\"{}\"", yaml_escape(x))).collect::<Vec<_>>().join(", ")));
    }
    s.push_str(&format!("language: {}\n", a.language));
    s.push_str(&format!("paywalled: {}\n", a.paywalled));
    s.push_str(&format!("word_count: {}\n", a.word_count));
    s.push_str(&format!("fetched_at: {}\n", a.fetched_at));
    if let Some(f) = a.audio.as_ref().and_then(|x| x.file.as_ref()) {
        s.push_str(&format!("audio: {f}\n"));
    }
    s.push_str(&format!("images: {}\n", a.images.len()));
    s.push_str("---\n\n");

    s.push_str(&format!("# {}\n\n", a.title));
    if !a.kicker.is_empty() {
        s.push_str(&format!("*{}*\n\n", a.kicker));
    }
    if !a.description.is_empty() {
        s.push_str(&format!("> {}\n\n", a.description));
    }
    let mut meta_line: Vec<String> = Vec::new();
    if !a.authors.is_empty() {
        meta_line.push(format!("von {}", a.authors.join(", ")));
    }
    if !a.published.is_empty() {
        meta_line.push(a.published.clone());
    }
    if !a.section.is_empty() {
        meta_line.push(a.section.clone());
    }
    if !meta_line.is_empty() {
        s.push_str(&meta_line.join(" · "));
        s.push_str("\n\n");
    }
    if let Some(au) = a.audio.as_ref() {
        let target = au.file.clone().unwrap_or_else(|| au.url.clone());
        let len = if au.duration_text.is_empty() { String::new() } else { format!(" ({})", au.duration_text) };
        s.push_str(&format!("🎧 [Artikel anhören{len}]({target})\n\n"));
    }
    if a.paywalled {
        s.push_str("> ⚠️ this article looks paywalled/truncated – log in via `--state state.json` for the full text\n\n");
    }

    for b in &a.blocks {
        match b {
            Block::Head { level, text } => {
                s.push_str(&format!("{} {}\n\n", "#".repeat((*level as usize).max(2)), text));
            }
            Block::Para { text } => s.push_str(&format!("{text}\n\n")),
            Block::Quote { text, source } => {
                s.push_str(&format!("> {text}\n"));
                if !source.is_empty() {
                    s.push_str(&format!(">\n> — {source}\n"));
                }
                s.push('\n');
            }
            Block::Item { text } => s.push_str(&format!("- {text}\n")),
            Block::Image { index } => {
                if let Some(img) = a.images.get(*index) {
                    let target = img.file.clone().unwrap_or_else(|| img.url.clone());
                    let alt = if img.alt.is_empty() { "Bild" } else { img.alt.as_str() };
                    s.push_str(&format!("![{alt}]({target})\n\n"));
                    if !img.caption.is_empty() {
                        s.push_str(&format!("*{}*\n\n", img.caption));
                    }
                    if !img.credit.is_empty() {
                        s.push_str(&format!("<sub>Foto: {}</sub>\n\n", img.credit));
                    }
                }
            }
        }
    }
    s
}

fn yaml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
