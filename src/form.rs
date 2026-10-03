//! Minimal HTML form parser, enough to replay the JSF login without a browser.
//!
//! The SSO page (`gruppenkonto.spiegel.de/anmelden.html`) is a plain
//! `application/x-www-form-urlencoded` POST with
//! `jakarta.faces.ViewState=stateless`, no captcha and no bot-protection
//! script, so the form can be replayed with ordinary requests:
//!
//! 1. GET the page, keep `loginform`, `_csrf`, `targetUrl`,
//!    `requestAccessToken`, `loginform:step`, `jakarta.faces.ViewState`
//! 2. POST `loginform:username` (+ hidden fields, + `loginform:submit`)
//! 3. the response is the second step (the password field becomes visible),
//!    so POST again with the password
//!
//! Values are never logged; only field names are.

use anyhow::Result;
use scraper::{ElementRef, Html, Selector};
use url::Url;

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub value: String,
    /// "hidden", "email", "password", "text", "submit"
    pub kind: String,
    /// false for `style="display:none"` / `type=hidden`
    pub visible: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Form {
    pub action: String,
    pub method: String,
    pub fields: Vec<Field>,
    /// name of the e-mail field, if the form has one
    pub email: Option<String>,
    /// name of the password field, if present at all
    pub password: Option<String>,
    /// is the password field actually shown in this step?
    pub password_visible: bool,
    /// name of the submit control
    pub submit: Option<String>,
    /// text of an error/alert box, if the page shows one
    pub error: Option<String>,
}

impl Form {
    /// Everything the browser would send on submit: hidden fields plus `extra`.
    pub fn body(&self, extra: &[(String, String)]) -> String {
        let mut pairs: Vec<(String, String)> = self
            .fields
            .iter()
            .filter(|f| f.kind == "hidden")
            .map(|f| (f.name.clone(), f.value.clone()))
            .collect();
        pairs.extend(extra.iter().cloned());
        if let Some(s) = &self.submit {
            pairs.push((s.clone(), String::new()));
        }
        encode(&pairs)
    }

    pub fn field_names(&self) -> Vec<String> {
        self.fields.iter().map(|f| format!("{}:{}", f.kind, f.name)).collect()
    }
}

/// Parse the first form that carries an e-mail or password input.
pub fn parse(html: &str, base: &Url) -> Option<Form> {
    let doc = Html::parse_document(html);
    let form_sel = Selector::parse("form").ok()?;
    for form in doc.select(&form_sel) {
        let mut f = Form {
            action: base
                .join(form.value().attr("action").unwrap_or(""))
                .map(|u| u.to_string())
                .unwrap_or_else(|_| base.to_string()),
            method: form.value().attr("method").unwrap_or("get").to_ascii_lowercase(),
            ..Default::default()
        };
        for input in form.select(&Selector::parse("input,button").ok()?) {
            let v = input.value();
            let Some(name) = v.attr("name").filter(|n| !n.is_empty()) else { continue };
            let kind = v.attr("type").unwrap_or(if v.name() == "button" { "submit" } else { "text" });
            let visible = kind != "hidden"
                && !v.attr("style").unwrap_or("").replace(' ', "").to_ascii_lowercase().contains("display:none");
            let label = label_of(&doc, input);
            if kind == "email" || label.to_ascii_lowercase().contains("e-mail") {
                f.email.get_or_insert(name.to_string());
            }
            if kind == "password" {
                f.password.get_or_insert(name.to_string());
                f.password_visible |= visible;
            }
            if kind == "submit" {
                f.submit.get_or_insert(name.to_string());
            }
            f.fields.push(Field {
                name: name.to_string(),
                value: v.attr("value").unwrap_or("").to_string(),
                kind: kind.to_string(),
                visible,
            });
        }
        if f.email.is_some() || f.password.is_some() {
            f.error = error_text(&doc);
            return Some(f);
        }
    }
    None
}

/// `<label for=id>`, `aria-label`, `<label>` ancestor or placeholder.
fn label_of(doc: &Html, input: ElementRef) -> String {
    let v = input.value();
    if let Some(aria) = v.attr("aria-label") {
        return aria.to_string();
    }
    if let Some(id) = v.attr("id") {
        if let Ok(sel) = Selector::parse(&format!("label[for=\"{id}\"]")) {
            if let Some(l) = doc.select(&sel).next() {
                return l.text().collect::<String>();
            }
        }
    }
    if let Some(l) = input.ancestors().find_map(|a| ElementRef::wrap(a)).filter(|a| a.value().name() == "label") {
        return l.text().collect::<String>();
    }
    v.attr("placeholder").unwrap_or("").to_string()
}

fn error_text(doc: &Html) -> Option<String> {
    let sel = Selector::parse(".error, [role=alert], .ui-messages-error, [class*=rror], .cms-message").ok()?;
    let t = doc.select(&sel).next()?.text().collect::<String>();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() { None } else { Some(t) }
}

/// `application/x-www-form-urlencoded` body.
pub fn encode(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent(k), percent(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Parse a form out of a page fetched over plain HTTP.
pub fn from_url_body(base: &str, body: &str) -> Result<Form> {
    let base = Url::parse(base)?;
    parse(body, &base).ok_or_else(|| anyhow::anyhow!("no login form found at {base}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSF: &str = r#"
      <form id="loginform" method="post" action="/anmelden.html"
            enctype="application/x-www-form-urlencoded" novalidate>
        <input type="hidden" name="loginform" value="loginform"/>
        <input type="hidden" name="_csrf" value="abc-123"/>
        <input type="hidden" name="targetUrl" value="https://www.spiegel.de/"/>
        <input type="hidden" name="requestAccessToken" value="true"/>
        <input type="hidden" name="loginform:step" value="anmelden"/>
        <label for="username">E-Mail-Adresse</label>
        <input type="email" name="loginform:username" id="username" value="loginname"/>
        <input type="password" name="password" style="display: none" aria-hidden="true"/>
        <button type="submit" name="loginform:submit" id="submit">Anmelden oder Konto erstellen</button>
      </form>
      <div class="ui-messages-error">Bitte geben Sie Ihre E-Mail-Adresse an.</div>"#;

    #[test]
    fn finds_fields_and_step() {
        let base = Url::parse("https://gruppenkonto.spiegel.de/anmelden.html").unwrap();
        let f = parse(JSF, &base).expect("form");
        assert_eq!(f.action, "https://gruppenkonto.spiegel.de/anmelden.html");
        assert_eq!(f.method, "post");
        assert_eq!(f.email.as_deref(), Some("loginform:username"));
        assert_eq!(f.password.as_deref(), Some("password"));
        assert!(!f.password_visible, "step 1 hides the password field");
        assert_eq!(f.submit.as_deref(), Some("loginform:submit"));
        assert_eq!(f.error.as_deref(), Some("Bitte geben Sie Ihre E-Mail-Adresse an."));
        assert_eq!(f.field_names().len(), 8);
    }

    #[test]
    fn body_sends_hidden_fields_plus_extra() {
        let base = Url::parse("https://x/anmelden.html").unwrap();
        let f = parse(JSF, &base).unwrap();
        let body = f.body(&[("loginform:username".into(), "a b@c.de".into())]);
        assert!(body.contains("_csrf=abc-123"));
        assert!(body.contains("jakarta.faces.ViewState") == false); // not in this fixture
        assert!(body.contains("loginform%3Ausername=a+b%40c.de"), "field names are percent-encoded: {body}");
        assert!(body.contains("loginform%3Astep=anmelden"));
        assert!(body.contains("loginform%3Asubmit="));
        assert!(!body.contains("password"), "step 1 must not send the password");
    }

    #[test]
    fn visible_password_is_detected() {
        let html = JSF.replace("style=\"display: none\"", "");
        let base = Url::parse("https://x/anmelden.html").unwrap();
        assert!(parse(&html, &base).unwrap().password_visible);
    }
}
