//! spiegel-crawler – download SPIEGEL articles (text + images + audio).
//!
//! Pure Rust: no Python, no Playwright, no WebDriver. For the parts that need a
//! real browser (the interactive login and the client-rendered Merkliste) it
//! drives the Chromium that is already installed, over the DevTools protocol.
//!
//! ```text
//! # 1. log in once (writes cookies.json + urls.txt with your Merkliste)
//! SPIEGEL_USER=you@example.com SPIEGEL_PASS=… cargo run --release -- --login --dry-run
//!
//! # 2. crawl everything
//! cargo run --release -- --urls-file urls.txt --out articles
//! ```
//!
//! Without a login it still crawls every article link found in `--list-url`
//! (RSS feeds, section pages) and all `--url`/`--urls-file` entries.

mod article;
mod auth;
mod browser;
mod fetch;
mod firefox;
mod store;
mod util;

use anyhow::{bail, Context, Result};
use article::Article;
use clap::Parser;
use fetch::Fetcher;
use rayon::prelude::*;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;
use store::Store;

const MERKLISTE: &str = "https://www.spiegel.de/fuermich/merkliste";

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

    /// Fetch this page (RSS feed, section page, Merkliste) and crawl every article link found
    #[arg(short = 'l', long = "list-url", value_name = "URL")]
    list_urls: Vec<String>,

    /// Log in with Chromium first, then dump the Merkliste to urls.txt + cookies.json
    #[arg(long)]
    login: bool,

    /// Merkliste URL to dump after --login
    #[arg(long = "merkliste-url", default_value = MERKLISTE, value_name = "URL")]
    merkliste_url: String,

    /// SPIEGEL e-mail (default: $SPIEGEL_USER)
    #[arg(long, value_name = "MAIL", env = "SPIEGEL_USER")]
    user: Option<String>,

    /// SPIEGEL password (default: $SPIEGEL_PASS – prefer the env var)
    #[arg(long, value_name = "PASS", env = "SPIEGEL_PASS", hide_env_values = true)]
    password: Option<String>,

    /// Show the browser window (useful while debugging the login)
    #[arg(long)]
    headed: bool,

    /// Chromium/Chrome binary to drive (default: auto-detect)
    #[arg(long, value_name = "PATH")]
    chrome: Option<PathBuf>,

    /// Session cookies as JSON (Playwright storage_state.json also works)
    #[arg(long, default_value = "cookies.json", value_name = "PATH")]
    cookies: PathBuf,

    /// Extra cookie as name=value (repeatable) – a login-free escape hatch
    #[arg(long = "cookie", value_name = "NAME=VALUE")]
    cookie_pairs: Vec<String>,

    /// Proxy for HTTP and the browser, e.g. http://host:3128
    #[arg(long, value_name = "URL", env = "SPIEGEL_PROXY")]
    proxy: Option<String>,

    /// Force the browser for listing pages even if plain HTTP finds links
    #[arg(long)]
    render: bool,

    /// Import the spiegel.de cookies from your Firefox profile (no login automation)
    #[arg(long = "from-firefox")]
    from_firefox: bool,

    /// Check whether the current cookies are a live login (reads the Merkliste) and exit
    #[arg(long = "check-session")]
    check_session: bool,

    /// Debug: render this URL in the browser, print the value of --eval, then exit (repeatable)
    #[arg(long = "dump-url", value_name = "URL")]
    dump_urls: Vec<String>,

    /// Debug: JS expression evaluated by --dump-url (may be async)
    #[arg(long, value_name = "JS", default_value = "document.title")]
    eval: String,

    /// Debug: click elements with this exact text before --eval (repeatable)
    #[arg(long = "click-text", value_name = "TEXT")]
    click_text: Vec<String>,

    /// Debug: run this JS step before --eval, in order (repeatable)
    #[arg(long = "js", value_name = "JS")]
    js_steps: Vec<String>,

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
    let mut discovered: Vec<String> = Vec::new();

    // ── 0. session source: cookies from the local Firefox profile ────────────
    if cli.from_firefox {
        let fresh = firefox::import_firefox()?;
        let mut state = if cli.cookies.exists() {
            auth::StorageState::load(&cli.cookies)?
        } else {
            auth::StorageState::from_cookies(Vec::new())
        };
        for c in fresh {
            if let Some(slot) = state.cookies.iter_mut().find(|e| e.name == c.name && e.domain == c.domain) {
                *slot = c;
            } else {
                state.cookies.push(c);
            }
        }
        state.save(&cli.cookies)?;
        println!("session: {} cookies -> {}", state.cookies.len(), cli.cookies.display());
    }

    // ── 1. optional browser login ────────────────────────────────────────────
    if cli.login {
        let user = cli
            .user
            .clone()
            .or_else(|| prompt("SPIEGEL e-mail: "))
            .context("no e-mail: pass --user or set $SPIEGEL_USER")?;
        let pass = cli
            .password
            .clone()
            .context("no password: set $SPIEGEL_PASS (or --password)")?;
        let chrome = match &cli.chrome {
            Some(p) => p.clone(),
            None => browser::locate_chrome().context(
                "no Chromium/Chrome found – set --chrome, or skip --login and use \
                 --cookies/--cookie with an existing session",
            )?,
        };
        println!("chrome: {}", chrome.display());
        let res = browser::login_and_dump(
            &chrome,
            &user,
            &pass,
            &cli.merkliste_url,
            cli.headed,
            cli.proxy.as_deref(),
            cli.verbose,
        )?;
        println!("merkliste: {} article link(s)", res.urls.len());
        if !res.logged_in {
            eprintln!("! login was not confirmed – rerun with --headed to see what the browser is doing");
        }
        let state = auth::StorageState::from_cookies(res.cookies);
        state.save(&cli.cookies)?;
        println!("wrote {} ({} cookies)", cli.cookies.display(), state.cookies.len());
        if res.logged_in {
            write_url_list("urls.txt", &res.urls)?;
            println!("wrote urls.txt ({} URLs)", res.urls.len());
        } else {
            eprintln!("! not writing urls.txt: the links on an unauthenticated Merkliste are not yours");
        }
        discovered = res.urls;
    }

    // ── 2. HTTP client with whatever session we have ─────────────────────────
    let mut jars = Vec::new();
    let mut session: Vec<auth::StorageCookie> = Vec::new();
    #[allow(unused_assignments)]
    if cli.cookies.exists() {
        let state = auth::StorageState::load(&cli.cookies)?;
        eprintln!("auth: {} cookies from {}", state.cookies.len(), cli.cookies.display());
        session = state.cookies.clone();
        jars.push(state.into_jar()?);
    } else if !cli.cookie_pairs.is_empty() {
        jars.push(auth::jar_from_pairs(&cli.cookie_pairs)?);
    } else {
        eprintln!(
            "warning: no session ({} not found) – crawling anonymously; \
             paywalled articles will be truncated. Use --login or --cookie.",
            cli.cookies.display()
        );
    }
    let fetcher = Fetcher::new(jars.pop(), cli.delay_ms, cli.verbose, cli.proxy.as_deref())?;

    // ── 1b. debug: render + eval ─────────────────────────────────────────────
    if !cli.dump_urls.is_empty() {
        let chrome = cli
            .chrome
            .clone()
            .or_else(browser::locate_chrome)
            .context("no Chromium/Chrome found")?;
        for url in &cli.dump_urls {
            let mut b = browser::Browser::launch(&chrome, cli.headed, cli.proxy.as_deref(), cli.verbose)?;
            if !session.is_empty() {
                let n = b.set_cookies(&session)?;
                if cli.verbose {
                    eprintln!("debug: {n} session cookies installed");
                }
            }
            b.goto(url, std::time::Duration::from_millis(2500))?;
            for t in &cli.click_text {
                println!("click {t:?}: {}", b.click_text(t)?);
                // the click may navigate: wait for the new document to settle
                let _ = b.wait_for("document.readyState === 'complete'", std::time::Duration::from_secs(20));
                std::thread::sleep(std::time::Duration::from_millis(3000));
            }
            for (i, step) in cli.js_steps.iter().enumerate() {
                let r = b.eval(step)?;
                println!("js[{i}]: {}", serde_json::to_string(&r)?);
                let _ = b.wait_for("document.readyState === 'complete'", std::time::Duration::from_secs(20));
                std::thread::sleep(std::time::Duration::from_millis(1500));
            }
            let v = b.eval(&cli.eval)?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        return Ok(());
    }

    // ── 1a. session check ────────────────────────────────────────────────────
    if cli.check_session {
        let hints: Vec<&str> = session
            .iter()
            .map(|c| c.name.as_str())
            .filter(|n| auth::SESSION_COOKIE_HINTS.contains(n))
            .collect();
        let (_, html) = fetcher.text(&cli.merkliste_url)?;
        let links = article::links_in_html(&html);
        println!("page:                 {}", cli.merkliste_url);
        println!(
            "cookies:              {} loaded; session cookies: {}",
            session.len(),
            if hints.is_empty() { "none".to_string() } else { hints.join(", ") }
        );
        println!("article links in HTML: {}", links.len());
        if links.is_empty() {
            println!(
                "\nthe list is not in the HTML – the page fetches it with JavaScript, so render it:\n  \
                 spiegel-crawler --list-url {} --render -o articles",
                cli.merkliste_url
            );
            std::process::exit(2);
        }
        for u in links.iter().take(10) {
            println!("  {u}");
        }
        return Ok(());
    }

    // ── 3. work list ─────────────────────────────────────────────────────────
    let mut urls = collect_urls(&cli, &fetcher, discovered, &session)?;
    if cli.limit > 0 && urls.len() > cli.limit {
        println!("limiting to the first {} of {} articles", cli.limit, urls.len());
        urls.truncate(cli.limit);
    }
    if urls.is_empty() {
        bail!("no article URLs (use --login, --url, --urls-file or --list-url)");
    }
    println!("{} article(s) to crawl", urls.len());
    if cli.dry_run {
        for u in &urls {
            println!("{u}");
        }
        return Ok(());
    }

    // ── 4. crawl ─────────────────────────────────────────────────────────────
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

/// Explicit URLs + URL file + links discovered on listing pages.
fn collect_urls(
    cli: &Cli,
    fetcher: &Fetcher,
    discovered: Vec<String>,
    session: &[auth::StorageCookie],
) -> Result<Vec<String>> {
    let mut set: BTreeSet<String> = BTreeSet::new();

    for u in cli.urls.iter().chain(discovered.iter()) {
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
        let mut found = http_links(fetcher, list)?;
        if found.is_empty() || cli.render {
            if let Some(chrome) = cli.chrome.clone().or_else(browser::locate_chrome) {
                println!("listing {list}: rendering in the browser…");
                match browser::render_links(&chrome, list, &session, list.contains("/fuermich/"), cli.headed, cli.proxy.as_deref(), cli.verbose) {
                    Ok(r) => {
                        found = r.urls;
                        // Logging in by hand in a --headed window still pays off:
                        // the session is exported for the HTTP crawl.  Never let an
                        // *anonymous* render overwrite a good session file.
                        if !auth::looks_logged_in(&r.cookies) {
                            eprintln!(
                                "  ! the browser holds no session cookie – leaving {} untouched",
                                cli.cookies.display()
                            );
                        } else if !r.cookies.is_empty() {
                            let mut state = if cli.cookies.exists() {
                                auth::StorageState::load(&cli.cookies)?
                            } else {
                                auth::StorageState::from_cookies(Vec::new())
                            };
                            for c in r.cookies {
                                if !state.cookies.iter().any(|e| e.name == c.name && e.domain == c.domain) {
                                    state.cookies.push(c);
                                }
                            }
                            state.save(&cli.cookies)?;
                            println!("  session: {} cookies -> {}", state.cookies.len(), cli.cookies.display());
                        }
                    }
                    Err(e) => eprintln!("  ! browser render failed: {e:#}"),
                }
            }
        }
        println!("listing {list}: {} article link(s)", found.len());
        if found.is_empty() {
            eprintln!(
                "  ! nothing found. A personal list needs a session: try --from-firefox \
                 (imports your Firefox cookies), then --check-session. Fallback: --login."
            );
        }
        for u in found {
            if let Some(u) = article::normalize_url(&u) {
                set.insert(u);
            }
        }
    }

    Ok(set.into_iter().collect())
}

fn http_links(fetcher: &Fetcher, url: &str) -> Result<Vec<String>> {
    let (_, body) = fetcher.text(url).with_context(|| format!("listing page {url}"))?;
    Ok(article::links_in_html(&body))
}

fn write_url_list(path: &str, urls: &[String]) -> Result<()> {
    let mut body = urls.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    std::fs::write(path, body).with_context(|| format!("writing {path}"))?;
    Ok(())
}

fn prompt(what: &str) -> Option<String> {
    use std::io::Write;
    print!("{what}");
    std::io::stdout().flush().ok()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}
