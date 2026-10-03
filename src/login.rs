//! Login with plain HTTP requests – no browser, no Playwright.
//!
//! The SSO page turned out to be a plain `POST application/x-www-form-urlencoded`
//! JSF form (`jakarta.faces.ViewState=stateless`) with no captcha and no
//! bot-protection script, so it can be replayed:
//!
//! * step 1: POST `loginform:username` + the hidden fields + `loginform:submit`
//! * step 2: the response shows the password field, POST again with it
//!
//! Success is only accepted when the server hands out a session cookie
//! (`accessInfo`, `sara_user_session`, …), exactly like the browser path.

use crate::auth::{self, StorageCookie};
use crate::fetch::Fetcher;
use crate::form::{self, Form};
use anyhow::{bail, Context, Result};
use std::time::Duration;
use url::Url;

pub const LOGIN_URL: &str =
    "https://gruppenkonto.spiegel.de/anmelden.html?targetUrl=https%3A%2F%2Fwww.spiegel.de%2F&requestAccessToken=true";

pub struct HttpLogin {
    pub cookies: Vec<StorageCookie>,
    pub logged_in: bool,
}

/// Fetch the login page and return its parsed form (used by `--parse-form` too).
pub fn fetch_form(fetcher: &Fetcher, url: &str) -> Result<Form> {
    let (final_url, body) = fetcher.text(url)?;
    form::from_url_body(&final_url, &body)
}

/// Replay the login form with ordinary requests.
pub fn login(
    fetcher: &Fetcher,
    user: &str,
    password: &str,
    verbose: bool,
) -> Result<HttpLogin> {
    let mut collected: Vec<StorageCookie> = Vec::new();
    let host = Url::parse(LOGIN_URL)?.host_str().unwrap_or("gruppenkonto.spiegel.de").to_string();

    let start = fetch_form(fetcher, LOGIN_URL)?;
    if verbose {
        eprintln!("http login: action={} method={}", start.action, start.method);
        eprintln!("http login: fields={:?}", start.field_names());
    }
    if !start.method.eq_ignore_ascii_case("post") {
        bail!("login form is not a POST form (method={})", start.method);
    }
    let email_field = start.email.clone().context("no e-mail field in the login form")?;

    let mut current = start;
    let mut sent_password = false;
    for round in 1..=3 {
        let mut extra: Vec<(String, String)> = vec![(email_field.clone(), user.to_string())];
        let send_password = current.password_visible && !sent_password;
        let sent_now = send_password;
        if send_password {
            let field = current.password.clone().context("no password field to fill")?;
            extra.push((field, password.to_string()));
        }
        let body = current.body(&extra);
        if verbose {
            eprintln!(
                "http login: round {round} POST {} ({} bytes, hidden+{}, password: {})",
                current.action,
                body.len(),
                if send_password { "e-mail+password" } else { "e-mail" },
                send_password
            );
        }
        let (final_url, html, status, set_cookies) = fetcher.post_form(&current.action, &body)?;
        for raw in &set_cookies {
            if let Some(c) = auth::parse_set_cookie(raw, &host) {
                if verbose {
                    eprintln!("http login:   set-cookie {}", c.name);
                }
                collected.retain(|e| e.name != c.name || e.domain != c.domain);
                collected.push(c);
            }
        }
        if !(200..400).contains(&status) {
            bail!("login POST returned HTTP {status} ({})", final_url);
        }
        if sent_now {
            sent_password = true;
            // after sending the password, success is a session cookie
            if auth::looks_logged_in(&collected) {
                return Ok(HttpLogin { cookies: collected, logged_in: true });
            }
        }
        // otherwise look at the next step
        let Ok(next) = form::from_url_body(&final_url, &html) else {
            // no form any more: maybe we are logged in, maybe redirected
            if auth::looks_logged_in(&collected) {
                return Ok(HttpLogin { cookies: collected, logged_in: true });
            }
            bail!("no login form in the response from {final_url}");
        };
        if verbose {
            eprintln!("http login: step fields={:?} password_visible={}", next.field_names(), next.password_visible);
        }
        if next.email.is_none() && next.password.is_none() {
            if auth::looks_logged_in(&collected) {
                return Ok(HttpLogin { cookies: collected, logged_in: true });
            }
            bail!("login form disappeared without a session cookie ({final_url})");
        }
        if sent_now {
            // password was sent and we are still on the form: it was rejected
            let why = next.error.unwrap_or_else(|| "no error message on the page".to_string());
            bail!("login rejected: {why}");
        }
        current = next;
        std::thread::sleep(Duration::from_millis(300));
    }
    bail!("login did not complete after 3 requests (session cookie missing)")
}
