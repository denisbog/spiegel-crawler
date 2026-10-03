//! Minimal Chrome DevTools Protocol client – the pure-Rust replacement for
//! Playwright.
//!
//! Only what this crawler needs is implemented: launch a local Chromium,
//! navigate, run JavaScript in the page, scroll a list, read cookies.
//! That is enough to (a) log in interactively once, (b) read the
//! client-rendered Merkliste, and (c) use the session for the fast HTTP crawl.
//!
//! No Python, no WebDriver, no extra browser download: it drives the Chromium
//! that Playwright (or the distribution) already put on the machine.

use crate::auth::StorageCookie;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Message, WebSocket};

type Ws = WebSocket<MaybeTlsStream<std::net::TcpStream>>;

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

/// Find a Chromium/Chrome binary we can drive.
pub fn locate_chrome() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SPIEGEL_CHROME") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    for p in [
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
        "/opt/google/chrome/chrome",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ] {
        if Path::new(p).is_file() {
            return Some(PathBuf::from(p));
        }
    }
    // Playwright's cache: chromium-<rev>/chrome-linux64/chrome (new) or chrome-linux/chrome
    let cache = std::env::var("PLAYWRIGHT_BROWSERS_PATH")
        .map(PathBuf::from)
        .ok()
        .or_else(|| Some(dirs_cache()?.join("ms-playwright")))?;
    let mut candidates: Vec<(u64, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&cache).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("chromium-") && !name.starts_with("chromium_headless_shell-") {
            continue;
        }
        let rev: u64 = name.rsplit('-').next().and_then(|r| r.parse().ok()).unwrap_or(0);
        for sub in ["chrome-linux64/chrome", "chrome-linux/chrome", "chrome-linux/headless_shell"] {
            let p = entry.path().join(sub);
            if p.is_file() {
                candidates.push((rev, p));
            }
        }
    }
    candidates.sort();
    candidates.pop().map(|(_, p)| p)
}

fn dirs_cache() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
}

pub struct Browser {
    child: Child,
    ws: Ws,
    next_id: u64,
    session: String,
    profile: PathBuf,
}

impl Browser {
    /// Launch Chromium and attach to a fresh page target.
    pub fn launch(chrome: &Path, headed: bool, proxy: Option<&str>, verbose: bool) -> Result<Self> {
        let port = {
            // ask the OS for a free port, then hand it to Chromium
            let l = TcpListener::bind("127.0.0.1:0")?;
            l.local_addr()?.port()
        };
        let profile = std::env::temp_dir().join(format!("spiegel-crawler-chrome-{port}-{}", std::process::id()));
        std::fs::create_dir_all(&profile)?;

        let mut cmd = Command::new(chrome);
        cmd.arg(format!("--remote-debugging-port={port}"))
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--no-sandbox")
            .arg("--disable-dev-shm-usage")
            .arg("--disable-gpu")
            .arg("--window-size=1400,1000")
            .arg(format!("--user-agent={UA}"))
            .arg("about:blank")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if !headed {
            cmd.arg("--headless=new");
        }
        if let Some(p) = proxy {
            cmd.arg(format!("--proxy-server={p}"));
        }
        let child = cmd.spawn().with_context(|| format!("launching {}", chrome.display()))?;

        // wait for the DevTools endpoint
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(700))
            .build()?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut ws_url = None;
        while Instant::now() < deadline {
            if let Ok(resp) = client.get(format!("{base}/json/version")).send() {
                if let Ok(v) = resp.json::<Value>() {
                    if let Some(u) = v.get("webSocketDebuggerUrl").and_then(Value::as_str) {
                        ws_url = Some(u.to_string());
                        break;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        let ws_url = ws_url.ok_or_else(|| anyhow!("Chromium did not expose a DevTools endpoint on {base}"))?;
        if verbose {
            eprintln!("browser: {ws_url}");
        }
        let (ws, _) = connect(ws_url.as_str()).context("connecting to Chromium DevTools")?;

        let mut b = Browser { child, ws, next_id: 0, session: String::new(), profile };
        let target = b.command("Target.createTarget", json!({"url": "about:blank"}), None)?;
        let target_id = target["targetId"].as_str().context("Target.createTarget: no targetId")?.to_string();
        let attached = b.command(
            "Target.attachToTarget",
            json!({"targetId": target_id, "flatten": true}),
            None,
        )?;
        b.session = attached["sessionId"].as_str().context("attachToTarget: no sessionId")?.to_string();
        b.command("Page.enable", json!({}), Some(b.session.clone()))?;
        b.command("Network.enable", json!({}), Some(b.session.clone()))?;
        b.command("Runtime.enable", json!({}), Some(b.session.clone()))?;
        Ok(b)
    }

    fn command(&mut self, method: &str, params: Value, session: Option<String>) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = &session {
            msg["sessionId"] = json!(s);
        }
        self.ws
            .send(Message::text(msg.to_string()))
            .with_context(|| format!("sending {method}"))?;

        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let raw = match self.ws.read() {
                Ok(Message::Text(t)) => t.to_string(),
                Ok(Message::Binary(b)) => String::from_utf8_lossy(&b).into_owned(),
                Ok(Message::Close(_)) => bail!("DevTools connection closed during {method}"),
                Ok(_) => continue,
                Err(e) => bail!("DevTools read error during {method}: {e}"),
            };
            let v: Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("id").and_then(Value::as_u64) != Some(id) {
                continue; // event or another command's reply
            }
            if let Some(err) = v.get("error") {
                bail!("{method} failed: {err}");
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
        bail!("timeout waiting for {method}")
    }

    /// Run JavaScript and return the value (must be JSON-serialisable).
    /// Tolerates the execution context being replaced by a navigation.
    pub fn eval(&mut self, expression: &str) -> Result<Value> {
        match self.eval_once(expression) {
            Err(e) if e.to_string().contains("navigated or closed") => {
                std::thread::sleep(Duration::from_millis(500));
                self.eval_once(expression)
            }
            other => other,
        }
    }

    fn eval_once(&mut self, expression: &str) -> Result<Value> {
        let r = self.command(
            "Runtime.evaluate",
            json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
            Some(self.session.clone()),
        )?;
        if let Some(details) = r.get("exceptionDetails") {
            bail!("JS error: {}", details.get("exception").and_then(|e| e.get("description")).and_then(Value::as_str).unwrap_or("unknown"));
        }
        Ok(r.get("result").and_then(|x| x.get("value")).cloned().unwrap_or(Value::Null))
    }

    /// Navigate and wait until the document is usable (+ settle time for JS).
    pub fn goto(&mut self, url: &str, settle: Duration) -> Result<()> {
        self.command("Page.navigate", json!({"url": url}), Some(self.session.clone()))?;
        let deadline = Instant::now() + Duration::from_secs(45);
        while Instant::now() < deadline {
            let state = self.eval("document.readyState").unwrap_or(Value::Null);
            if state.as_str() == Some("complete") {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        std::thread::sleep(settle);
        Ok(())
    }

    /// Click the first element whose trimmed text matches (real DOM click).
    pub fn click_text(&mut self, text: &str) -> Result<bool> {
        let js = format!(
            r#"(() => {{ const els = [...document.querySelectorAll('a,button,[role=button],summary')];
                 const e = els.find(x => x.textContent.trim() === {t});
                 if (!e) return false;
                 for (const type of ['pointerdown','mousedown','pointerup','mouseup','click'])
                   e.dispatchEvent(new MouseEvent(type, {{bubbles: true, cancelable: true, view: window}}));
                 return true; }})()"#,
            t = serde_json::to_string(text)?
        );
        Ok(self.eval(&js)?.as_bool().unwrap_or(false))
    }

    /// Poll `condition` (a JS expression) until it is truthy.
    pub fn wait_for(&mut self, condition: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.eval(condition).map(|v| v.as_bool().unwrap_or(false)).unwrap_or(false) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        false
    }

    /// All article links currently in the DOM, normalised.
    ///
    /// `scope_main` restricts the search to `<main>`, which is what separates
    /// article lists from the header/footer furniture (never the user's list).
    pub fn article_links(&mut self, scope_main: bool) -> Result<Vec<String>> {
        let scope = if scope_main {
            "const s = document.querySelector('main'); if (!s) return [];"
        } else {
            "const s = document.querySelector('main') || document.body;"
        };
        let js = format!(
            r#"(() => {{ {scope}
                 return [...s.querySelectorAll("a[href*='-a-']")].map(a => a.href); }})()
                 .filter(h => /https:\/\/www\.spiegel\.de\/.*-a-[0-9a-f-]{{8,}}/.test(h))
                 .map(h => h.split('#')[0].split('?')[0])"#
        );
        let v = self.eval(&js)?;
        let mut out: Vec<String> = v
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Scroll to the bottom until the link count stops growing.
    pub fn scroll_all_articles(&mut self, verbose: bool, scope_main: bool) -> Result<Vec<String>> {
        let mut seen = self.article_links(scope_main)?;
        let mut stable = 0;
        for round in 0..80 {
            self.eval("window.scrollTo(0, document.body.scrollHeight)")?;
            // some lists need an explicit "load more" click
            self.eval(
                r#"(() => { for (const t of ["Mehr laden","Weitere Artikel","Mehr anzeigen"]) {
                       const b = [...document.querySelectorAll('button,a')].find(e => e.textContent.trim() === t);
                       if (b) { b.click(); return t; } } return null; })()"#,
            )?;
            std::thread::sleep(Duration::from_millis(900));
            let now = self.article_links(scope_main)?;
            if verbose && round % 5 == 0 {
                eprintln!("  scroll: {} links", now.len());
            }
            if now.len() == seen.len() {
                stable += 1;
                if stable >= 3 {
                    break;
                }
            } else {
                stable = 0;
                seen = now;
            }
        }
        if seen.is_empty() && scope_main && verbose {
            let elsewhere = self.article_links(false).unwrap_or_default().len();
            if elsewhere > 0 {
                eprintln!("  (nothing inside <main>; {elsewhere} link(s) elsewhere are site furniture)");
            }
        }
        Ok(seen)
    }

    /// One-line summary of what the current page looks like – used when a login
    /// step fails, so the user knows whether it was a captcha, 2FA or a typo.
    /// Never includes input values.
    pub fn diagnose(&mut self) -> String {
        let js = r#"(() => {
            const flags = [];
            if (document.querySelector('iframe[src*="recaptcha"], iframe[src*="hcaptcha"], .g-recaptcha, [class*=captcha]')) flags.push('captcha');
            if ([...document.querySelectorAll('input')].some(i => /otp|tan|code|token/i.test((i.name||'') + (i.id||'')))) flags.push('2FA field');
            const err = document.querySelector('.error, [role=alert], .ui-messages-error, [class*=rror]');
            return JSON.stringify({
                url: location.href,
                title: document.title,
                flags,
                inputs: [...document.querySelectorAll('input')].map(i => (i.type||'') + ':' + (i.name || i.id || '?')).slice(0, 12),
                message: err ? err.textContent.trim().slice(0, 200) : document.body.innerText.replace(/\s+/g, ' ').slice(0, 200)
            });
        })()"#;
        self.eval(js).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
    }

    /// Install session cookies into the browser (Network.setCookies).
    pub fn set_cookies(&mut self, cookies: &[StorageCookie]) -> Result<usize> {
        if cookies.is_empty() {
            return Ok(0);
        }
        let payload: Vec<Value> = cookies
            .iter()
            .map(|c| {
                json!({
                    "name": c.name,
                    "value": c.value,
                    "domain": c.domain,
                    "path": if c.path.is_empty() { "/" } else { &c.path },
                    "secure": c.secure,
                    "httpOnly": c.http_only,
                })
            })
            .collect();
        let n = payload.len();
        self.command("Network.setCookies", json!({"cookies": payload}), Some(self.session.clone()))?;
        Ok(n)
    }

    pub fn cookies(&mut self, urls: &[&str]) -> Result<Vec<StorageCookie>> {
        let r = self.command("Network.getCookies", json!({"urls": urls}), Some(self.session.clone()))?;
        let arr = r.get("cookies").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(arr
            .iter()
            .map(|c| StorageCookie {
                name: c.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                value: c.get("value").and_then(Value::as_str).unwrap_or_default().to_string(),
                domain: c.get("domain").and_then(Value::as_str).unwrap_or_default().to_string(),
                path: c.get("path").and_then(Value::as_str).unwrap_or("/").to_string(),
                secure: c.get("secure").and_then(Value::as_bool).unwrap_or(false),
                http_only: c.get("httpOnly").and_then(Value::as_bool).unwrap_or(false),
            })
            .collect())
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.ws.close(None);
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

// ------------------------------------------------------------------ login

/// The SSO form lives on a separate host (JSF app).  `targetUrl` sends us back
/// to spiegel.de after a successful login.
const LOGIN_URL: &str =
    "https://gruppenkonto.spiegel.de/anmelden.html?targetUrl=https%3A%2F%2Fwww.spiegel.de%2F&requestAccessToken=true";

/// Fields are labelled with `<label>`, not aria-label/placeholder.
const FIND_FIELD_JS: &str = r#"
    const labelOf = (i) => ((i.labels && i.labels[0] && i.labels[0].textContent) || "")
        + " " + (i.getAttribute("aria-label") || "") + " " + (i.name || "") + " " + (i.placeholder || "");
"#;

pub struct LoginResult {
    pub cookies: Vec<StorageCookie>,
    pub urls: Vec<String>,
    pub logged_in: bool,
}

/// Log in on spiegel.de and dump the Merkliste.
///
/// Mirrors a recorded Playwright session: open the login form, fill
/// e-mail + password, submit, then read /fuermich/merkliste.
pub fn login_and_dump(
    chrome: &Path,
    user: &str,
    password: &str,
    merkliste_url: &str,
    headed: bool,
    proxy: Option<&str>,
    verbose: bool,
) -> Result<LoginResult> {
    let mut b = Browser::launch(chrome, headed, proxy, verbose)?;

    // 1. login form. The SSO shows e-mail and password either together or in two
    //    steps (the recorded session pressed Enter after the e-mail first), so we
    //    accept both.
    if verbose {
        eprintln!("login: opening {LOGIN_URL}");
    }
    let email_present = format!(
        r#"(() => {{ {FIND_FIELD_JS}
             return !![...document.querySelectorAll('input')].find(i => /E-Mail/i.test(labelOf(i))); }})()"#
    );
    let password_present =
        r#"document.querySelectorAll('input[type=password]').length > 0"#.to_string();

    b.goto(LOGIN_URL, Duration::from_millis(1200))?;
    if !b.wait_for(&email_present, Duration::from_secs(25)) {
        if verbose {
            eprintln!("login: no form on {LOGIN_URL}, trying the homepage link");
        }
        b.goto("https://www.spiegel.de/", Duration::from_millis(1500))?;
        b.click_text("Anmelden")?;
        if !b.wait_for(&email_present, Duration::from_secs(25)) {
            bail!("login form not found. page: {}", b.diagnose());
        }
    }

    // fill one field, matched by its <label>/aria-label/name/placeholder
    fn fill_js(re: &str, value: &str, fallback_password: bool) -> Result<String> {
        Ok(format!(
            r#"(() => {{ {FIND_FIELD_JS}
                 const el = [...document.querySelectorAll("input")].find(i => /{re}/i.test(labelOf(i)))
                     || {fallback};
                 if (!el) return false;
                 const setter = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el), "value").set;
                 setter.call(el, {value});
                 el.dispatchEvent(new Event("input", {{bubbles: true}}));
                 el.dispatchEvent(new Event("change", {{bubbles: true}}));
                 el.focus();
                 return true; }})()"#,
            value = serde_json::to_string(value)?,
            fallback = if fallback_password { "document.querySelector('input[type=password]')" } else { "null" },
        ))
    }

    // submit the current step
    fn submit_js() -> &'static str {
        r#"(() => { const btn = [...document.querySelectorAll('button,input[type=submit]')]
                     .find(x => /anmelden|login|einloggen|weiter|konto erstellen/i.test(x.textContent || x.value || ''));
                   if (btn) { btn.click(); return 'button'; }
                   const f = document.querySelector('input[type=password], input[type=email]');
                   if (f && f.form) { f.form.requestSubmit(); return 'submit'; }
                   return 'nothing'; })()"#
    }

    if verbose {
        eprintln!("login: filling the e-mail field");
    }
    if b.eval(&fill_js("E-Mail", user, false)?)?.as_bool() != Some(true) {
        bail!("could not fill the e-mail field. page: {}", b.diagnose());
    }

    if b.eval(&password_present)?.as_bool() != Some(true) {
        // two-step variant: submit the e-mail, then wait for the password
        if verbose {
            eprintln!("login: two-step form, asking for the password");
        }
        b.eval(submit_js())?;
        if !b.wait_for(&password_present, Duration::from_secs(25)) {
            bail!("password field did not appear. page: {}", b.diagnose());
        }
    }
    if verbose {
        eprintln!("login: filling the password");
    }
    if b.eval(&fill_js("Passwort|Password", password, true)?)?.as_bool() != Some(true) {
        bail!("could not fill the password field. page: {}", b.diagnose());
    }
    b.eval(submit_js())?;

    // 4. back to spiegel.de: the SSO sets the session cookies only on success
    b.goto("https://www.spiegel.de/", Duration::from_millis(2500))?;
    let mut logged_in = false;
    for _ in 0..20 {
        let cookies = b.cookies(&["https://www.spiegel.de/"]).unwrap_or_default();
        if crate::auth::looks_logged_in(&cookies) {
            logged_in = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
    if logged_in {
        println!("login: ok");
    } else {
        eprintln!("! login was not confirmed – no session cookie was set");
        eprintln!("  page: {}", b.diagnose());
        eprintln!("  hint: rerun with --headed to watch the form (2FA, captcha, wrong password?)");
    }

    // 5. the Merkliste
    if verbose {
        eprintln!("login: reading {merkliste_url}");
    }
    b.goto(merkliste_url, Duration::from_millis(2500))?;
    let urls = b.scroll_all_articles(verbose, true)?;
    let cookies = b.cookies(&["https://www.spiegel.de/", merkliste_url])?;
    Ok(LoginResult { cookies, urls, logged_in })
}

pub struct RenderResult {
    pub urls: Vec<String>,
    /// cookies the browser holds for spiegel.de – lets a manual login inside a
    /// `--headed` window be exported for the fast HTTP crawl
    pub cookies: Vec<StorageCookie>,
}

/// Render any page in the browser, return its article links and cookies.
pub fn render_links(
    chrome: &Path,
    url: &str,
    session: &[StorageCookie],
    personal: bool,
    headed: bool,
    proxy: Option<&str>,
    verbose: bool,
) -> Result<RenderResult> {
    let mut b = Browser::launch(chrome, headed, proxy, verbose)?;
    // authenticate the render: the Merkliste is fetched by the page's own JS,
    // so the browser needs the session, not just our HTTP client
    match b.set_cookies(session) {
        Ok(n) if n > 0 && verbose => eprintln!("browser: {n} session cookies installed"),
        Ok(_) => {}
        Err(e) => eprintln!("  ! could not install session cookies: {e:#}"),
    }
    b.goto(url, Duration::from_millis(2000))?;
    let urls = b.scroll_all_articles(verbose, personal)?;
    let cookies = b.cookies(&["https://www.spiegel.de/", url])?;
    if verbose {
        let hints: Vec<&str> = cookies
            .iter()
            .map(|c| c.name.as_str())
            .filter(|n| crate::auth::SESSION_COOKIE_HINTS.contains(n))
            .collect();
        eprintln!(
            "browser: {} cookies, session cookies: {}",
            cookies.len(),
            if hints.is_empty() { "none".to_string() } else { hints.join(", ") }
        );
    }
    Ok(RenderResult { urls, cookies })
}
