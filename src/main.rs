//! spiegel-crawler – download SPIEGEL articles (text + images + audio).
//!
//! Two entry points:
//!
//! * `--login-http`  log in with plain HTTP requests (no browser) and save the
//!                   session to `cookies.json`
//! * `--list-url`    crawl every article of a page: feeds and section pages via
//!                   their HTML, your own lists ("Ihre Artikel") via the JSON API
//!
//! ```text
//! spiegel-crawler --login-http                       # prompts for e-mail + password
//! spiegel-crawler --list-url https://www.spiegel.de/fuermich/merkliste -o articles
//! spiegel-crawler --list-url https://www.spiegel.de/politik/index.rss -o articles
//! ```

mod api;
mod article;
mod auth;
mod fetch;
mod form;
mod login;
mod store;
mod util;

use anyhow::{bail, Context, Result};
use article::Article;
use clap::Parser;
use fetch::Fetcher;
use rayon::prelude::*;
use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use store::Store;

#[derive(Parser, Debug)]
#[command(
    name = "spiegel-crawler",
    version,
    about = "Download SPIEGEL articles (text, images, audio) from a URL list, a feed or your Merkliste"
)]
struct Cli {
    /// Article URL (repeatable)
    #[arg(short = 'u', long = "url", value_name = "URL")]
    urls: Vec<String>,

    /// File with one article URL per line ('#' starts a comment)
    #[arg(short = 'f', long = "urls-file", value_name = "PATH")]
    urls_file: Option<PathBuf>,

    /// Crawl every article link of this page: RSS feed, section page, Merkliste
    #[arg(short = 'l', long = "list-url", value_name = "URL")]
    list_urls: Vec<String>,

    /// Log in with plain HTTP requests (no browser) and save the session
    #[arg(long = "login-http")]
    login_http: bool,

    /// SPIEGEL e-mail (default: $SPIEGEL_USER)
    #[arg(long, value_name = "MAIL", env = "SPIEGEL_USER")]
    user: Option<String>,

    /// SPIEGEL password (default: $SPIEGEL_PASS, otherwise asked for without echo)
    #[arg(long, value_name = "PASS", env = "SPIEGEL_PASS", hide_env_values = true)]
    password: Option<String>,

    /// Session cookies as JSON (written by --login-http)
    #[arg(long, default_value = "cookies.json", value_name = "PATH")]
    cookies: PathBuf,

    /// Extra cookie as name=value (repeatable) – a manual escape hatch
    #[arg(long = "cookie", value_name = "NAME=VALUE")]
    cookie_pairs: Vec<String>,

    /// Check whether the current cookies still hold a session, then exit
    #[arg(long = "check-session")]
    check_session: bool,

    /// Proxy for HTTP requests, e.g. http://host:3128
    #[arg(long, value_name = "URL", env = "SPIEGEL_PROXY")]
    proxy: Option<String>,

    /// Output directory
    #[arg(short = 'o', long = "out", default_value = "articles", value_name = "DIR")]
    out: PathBuf,

    /// Articles fetched in parallel
    #[arg(short = 'j', long = "jobs", default_value_t = 3)]
    jobs: usize,

    /// Politeness delay between HTTP requests
    #[arg(long = "delay-ms", default_value_t = 300)]
    delay_ms: u64,

    /// Do not download images
    #[arg(long = "no-images")]
    no_images: bool,

    /// Do not download audio
    #[arg(long = "no-audio")]
    no_audio: bool,

    /// Re-download media even if the file already exists
    #[arg(long)]
    force: bool,

    /// Accept a Merkliste that comes back shorter than expected
    #[arg(long = "allow-partial")]
    allow_partial: bool,

    /// Only resolve and print the URL list
    #[arg(long = "dry-run")]
    dry_run: bool,

    /// Stop after N articles (0 = no limit)
    #[arg(short = 'n', long = "limit", default_value_t = 0)]
    limit: usize,

    #[arg(short = 'v', long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // ── 1. log in (plain HTTP) ───────────────────────────────────────────────
    if cli.login_http {
        let user = match cli.user.clone() {
            Some(u) => u,
            None => prompt_line("SPIEGEL e-mail: ")?.context("no e-mail given")?,
        };
        let pass = match cli.password.clone() {
            Some(p) => p,
            None => prompt_password("SPIEGEL password: ")?,
        };
        if pass.is_empty() {
            bail!("empty password");
        }

        // a jar, because the two JSF steps must carry each other's cookies
        let jar = Arc::new(reqwest::cookie::Jar::default());
        let probe = Fetcher::new(Some(jar), cli.delay_ms, cli.verbose, cli.proxy.as_deref())?;
        let res = login::login(&probe, &user, &pass, cli.verbose)?;
        if !res.logged_in {
            bail!("login finished without a session cookie");
        }
        let mut state = if cli.cookies.exists() {
            auth::StorageState::load(&cli.cookies)?
        } else {
            auth::StorageState::from_cookies(Vec::new())
        };
        for c in res.cookies {
            if let Some(slot) = state.cookies.iter_mut().find(|e| e.name == c.name && e.domain == c.domain) {
                *slot = c;
            } else {
                state.cookies.push(c);
            }
        }
        state.save(&cli.cookies)?;
        println!(
            "login: ok (plain HTTP, no browser) – {} cookies -> {}",
            state.cookies.len(),
            cli.cookies.display()
        );
        println!("next: --list-url https://www.spiegel.de/fuermich/merkliste -o articles");
        return Ok(());
    }

    // ── 2. HTTP client with the session we have ──────────────────────────────
    let jar = Arc::new(reqwest::cookie::Jar::default());
    if cli.cookies.exists() {
        let state = auth::StorageState::load(&cli.cookies)?;
        eprintln!("auth: {} cookies from {}", state.cookies.len(), cli.cookies.display());
        for c in &state.cookies {
            auth::add_cookie(&jar, c);
        }
    } else if cli.cookie_pairs.is_empty() {
        eprintln!(
            "warning: no session ({} not found) – crawling anonymously; paywalled articles \
             will be truncated. Run --login-http first.",
            cli.cookies.display()
        );
    }
    for p in &cli.cookie_pairs {
        let Some((name, value)) = p.split_once('=') else {
            bail!("--cookie wants name=value, got {p:?}");
        };
        auth::add_raw_cookie(&jar, name.trim(), value);
    }
    let fetcher = Fetcher::new(Some(jar), cli.delay_ms, cli.verbose, cli.proxy.as_deref())?;

    // ── 3. session check, if asked ───────────────────────────────────────────
    if cli.check_session {
        let hints = fetcher.session_cookie_names();
        let marks = api::bookmarks(&fetcher, cli.allow_partial);
        println!("session cookies: {}", if hints.is_empty() { "none".to_string() } else { hints.join(", ") });
        match marks {
            Ok(m) => println!("merkliste: {} of {} bookmark(s)", m.items.len(), m.requested),
            Err(e) => {
                println!("merkliste: not available ({e})");
                println!("\nRun --login-http to get a session.");
                std::process::exit(2);
            }
        }
        if hints.is_empty() {
            std::process::exit(2);
        }
        return Ok(());
    }

    // ── 4. work list ─────────────────────────────────────────────────────────
    let WorkList { mut urls, failed_listings } = collect_urls(&cli, &fetcher)?;
    for l in &failed_listings {
        eprintln!("! listing failed: {l}");
    }
    if cli.limit > 0 && urls.len() > cli.limit {
        println!("limiting to the first {} of {} articles", cli.limit, urls.len());
        urls.truncate(cli.limit);
    }
    if urls.is_empty() {
        if !failed_listings.is_empty() {
            std::process::exit(2);
        }
        bail!("no article URLs (use --login-http and/or --list-url, --url, --urls-file)");
    }
    println!("{} article(s) to crawl", urls.len());
    if cli.dry_run {
        for u in &urls {
            println!("{u}");
        }
        return Ok(());
    }

    // ── 5. crawl ─────────────────────────────────────────────────────────────
    let store = Store::new(&cli.out, !cli.no_images, !cli.no_audio, cli.force);
    std::fs::create_dir_all(&cli.out).with_context(|| format!("creating {}", cli.out.display()))?;

    let failures: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
    let pool = rayon::ThreadPoolBuilder::new().num_threads(cli.jobs.max(1)).build()?;

    pool.install(|| {
        urls.par_iter().for_each(|url| {
            println!("→ {url}");
            match crawl_one(&fetcher, &store, url) {
                Ok((dir, a)) => {
                    let mut bits = vec![format!("{} words", a.word_count)];
                    if !a.images.is_empty() {
                        bits.push(format!("{} images", a.images.len()));
                    }
                    if let Some(au) = &a.audio {
                        bits.push(format!("audio {}", au.duration_text));
                    }
                    if a.paywalled {
                        bits.push("PAYWALLED".into());
                    }
                    if !a.has_body() {
                        bits.push("NO BODY".into());
                    }
                    println!("✓ {} [{}]", dir.display(), bits.join(", "));
                    for w in &a.warnings {
                        eprintln!("  ! {}: {w}", a.id);
                    }
                }
                Err(e) => {
                    eprintln!("✗ {url}: {e:#}");
                    failures.lock().unwrap().push((url.clone(), format!("{e:#}")));
                }
            }
        });
    });

    let failures = failures.into_inner().unwrap();
    println!("\ndone: {} ok, {} failed", urls.len() - failures.len(), failures.len());
    for (u, e) in &failures {
        eprintln!("  failed {u}: {e}");
    }
    if !failures.is_empty() {
        std::process::exit(1);
    }
    if !failed_listings.is_empty() {
        // crawled what we could, but the run was incomplete: let scripts notice
        eprintln!("! {} listing(s) produced nothing – exiting non-zero", failed_listings.len());
        std::process::exit(2);
    }
    Ok(())
}

fn crawl_one(fetcher: &Fetcher, store: &Store, url: &str) -> Result<(PathBuf, Article)> {
    let (final_url, html) = fetcher.text(url)?;
    let mut a = article::parse(url, &html, &now());
    if a.canonical_url.is_empty() {
        a.canonical_url = final_url;
    }
    let dir = store.save(fetcher, &mut a)?;
    Ok((dir, a))
}

/// The work list plus the listings that produced nothing.
struct WorkList {
    urls: Vec<String>,
    failed_listings: Vec<String>,
}

/// Explicit URLs + URL file + links discovered on listing pages.
fn collect_urls(cli: &Cli, fetcher: &Fetcher) -> Result<WorkList> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    let mut failed_listings: Vec<String> = Vec::new();

    for u in &cli.urls {
        match article::normalize_url(u) {
            Some(u) => {
                set.insert(u);
            }
            None => eprintln!("skipping non-spiegel URL: {u}"),
        }
    }

    if let Some(path) = &cli.urls_file {
        let raw = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match article::normalize_url(line) {
                Some(u) => {
                    set.insert(u);
                }
                None => eprintln!("skipping line in {}: {line}", path.display()),
            }
        }
    }

    for list in &cli.list_urls {
        // Personal lists are backed by a JSON API – one request, no scraping.
        if is_personal(list) {
            match api::bookmarks(fetcher, cli.allow_partial) {
                Ok(b) if !b.items.is_empty() => {
                    println!("listing {list}: {} of {} bookmark(s)", b.items.len(), b.requested);
                    for m in &b.items {
                        println!(
                            "  {} [{}]",
                            m.title,
                            if m.access_level.is_empty() { "?" } else { &m.access_level }
                        );
                    }
                    for m in b.items {
                        if let Some(u) = article::normalize_url(&m.url) {
                            set.insert(u);
                        }
                    }
                    continue;
                }
                Ok(_) => {
                    eprintln!("  ! the bookmarks API reports an empty list");
                    failed_listings.push(format!("{list} (empty)"));
                }
                Err(e) => {
                    eprintln!("  ! bookmarks API failed: {e:#}");
                    failed_listings.push(format!("{list} ({e})"));
                }
            }
            continue;
        }

        let found = http_links(fetcher, list)?;
        println!("listing {list}: {} article link(s)", found.len());
        if found.is_empty() {
            eprintln!("  ! no article links found on that page");
            failed_listings.push(format!("{list} (no links)"));
        }
        for u in found {
            if let Some(u) = article::normalize_url(&u) {
                set.insert(u);
            }
        }
    }

    Ok(WorkList { urls: set.into_iter().collect(), failed_listings })
}

/// Pages under /fuermich/ are "mine": they are backed by the bookmarks API.
fn is_personal(url: &str) -> bool {
    url.contains("/fuermich/")
}

fn http_links(fetcher: &Fetcher, url: &str) -> Result<Vec<String>> {
    let (_, body) = fetcher.text(url).with_context(|| format!("listing page {url}"))?;
    Ok(article::links_in_html(&body))
}

/// Prompt on the terminal (visible input), e.g. for the e-mail address.
fn prompt_line(what: &str) -> Result<Option<String>> {
    use std::io::Write;
    print!("{what}");
    std::io::stdout().flush().ok();
    let mut s = String::new();
    if std::io::stdin().read_line(&mut s)? == 0 {
        return Ok(None); // EOF (no terminal attached)
    }
    let s = s.trim().to_string();
    Ok(if s.is_empty() { None } else { Some(s) })
}

/// Prompt for a secret without echoing it.
fn prompt_password(what: &str) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "no terminal to ask for the password on – pass --password or set $SPIEGEL_PASS"
        );
    }
    Ok(rpassword::prompt_password(what)?.trim().to_string())
}

fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}
