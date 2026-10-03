# spiegel-crawler

Downloads SPIEGEL articles — **text, images and audio** — from a URL list, an RSS
feed, a section page or your personal Merkliste. Pure Rust: no Python, no
Playwright, no WebDriver.

```bash
cargo build --release

# a) log in with your account – no browser import, it prompts for e-mail + password
./target/release/spiegel-crawler --login --dry-run
#    -> cookies.json (session) + urls.txt ("Ihre Artikel"), then crawl it:
./target/release/spiegel-crawler --urls-file urls.txt -o articles-ihre-artikel
#    (or crawl the list straight from the page, which needs the browser:)
./target/release/spiegel-crawler --list-url https://www.spiegel.de/fuermich/merkliste \
    --render -o articles-ihre-artikel

# b) the same login with plain requests only – no browser at all
./target/release/spiegel-crawler --login-http

# c) alternative without any credentials: reuse the session of your own browser
./target/release/spiegel-crawler --from-firefox

# d) feeds / section pages need no session at all
./target/release/spiegel-crawler --list-url https://www.spiegel.de/schlagzeilen/index.rss -n 5 -o articles
```

## What you get

```
articles/2026-10-03_papier-der-fraktionsspitze-gruene-suchen-gruende-…/
├── article.md      title, lead, body, pull quotes, inline images, audio link
├── article.json    the same as structured data (blocks, images[], audio, flags)
├── images/01_52adfd72-…_w1920.webp
└── audio/papier-der-fraktionsspitze-…​.mp3
```

Re-running is cheap: existing non-empty media files are skipped, and interrupted
downloads resume (`*.part` + HTTP range requests).

## Why this shape

| Phase | How | Why |
|---|---|---|
| Article fetch | `reqwest` + `scraper` (no JS) | article pages are fully server-rendered |
| Listing (RSS, sections) | plain HTTP | the links are in the HTML |
| Listing (Merkliste) | `GET /services/depot/api/v1/bookmarks` then `GET /services/sitesearch/fetch?ids=…` (`src/api.rs`) | the page has no article links in its HTML — it fetches this API with JS. Two plain requests give the list, so no browser is involved. Found by capturing the page's network log (`--dump-network`) |
| Listing (Merkliste), browser | headless Chromium over CDP, session cookies installed with `Network.setCookies` | fallback for `--render`, and the way `--login` reads the list |
| Session | `--from-firefox` reads `cookies.sqlite` (pure Rust, via `rusqlite`) | if you are logged in in your own browser you already have a session — no need to automate a login at all |
| Login (plain HTTP) | `POST gruppenkonto.spiegel.de/anmelden.html` (`src/login.rs`, `src/form.rs`) | the SSO form is a plain `application/x-www-form-urlencoded` POST with `jakarta.faces.ViewState=stateless`, no captcha and no bot-protection script — so it can be replayed |
| Login (browser) | headless Chromium over CDP | kept as `--login` for the case where the HTTP replay trips (2FA, a form change) |
| Crawl | `reqwest` + the session cookies from the browser | one browser visit, then hundreds of fast HTTP requests |

The CDP client (`src/browser.rs`) is ~300 lines of `tungstenite` + `serde_json`:
launch, `Page.navigate`, `Runtime.evaluate`, `Network.getCookies`. No extra
browser download — it uses the Chromium that Playwright or the distribution
already installed (`~/.cache/ms-playwright/chromium-*/chrome-linux64/chrome`,
`/usr/bin/google-chrome`, …; override with `--chrome`).

## Where the data comes from

| Data | Source in the page |
|---|---|
| headline, authors, dates, section, tags | `<script type="application/ld+json">` (schema.org `NewsArticle`) |
| lead image (largest variant) | `image`/`thumbnailUrl` in that JSON-LD, `og:image` |
| document id, language, canonical URL | `<script type="application/settings+json">` → `app.pageContext`, `page.info` |
| **audio** | the same settings JSON → `app.pageContext.clip.audioUrl` / `downloadUrl` / `duration` (CDN: Omny/Triton Digital), with `<audio src>` and `.mp3` regex as fallbacks |
| body text, headings, quotes, lists | the rendered `<article>`, containers marked `data-area="text"`, `"body"`, `"quote"`, `"intro"` |
| images in the body | `srcset`/`data-srcset` and `<picture><source>`; the widest candidate wins, duplicates are collapsed by image UUID |
| paywall state | `paywall.attributes.is_active`, `isAccessibleForFree`, truncation markers (`"Weiterlesen mit SPIEGEL+"`) |

Adverts, "related articles", gift boxes (`data-component="gift-box"`), breadcrumb
nav and other furniture are filtered out (see `SKIP_AREAS`/`SKIP_CLASSES`/
`SKIP_COMPONENTS` in `src/article.rs`).

## Options

| Flag | Meaning |
|---|---|
| `-u, --url <URL>` | article URL (repeatable) |
| `-f, --urls-file <PATH>` | one URL per line, `#` comments |
| `-l, --list-url <URL>` | crawl every article link found on this page (RSS, section, Merkliste) |
| `--from-firefox` | import the `spiegel.de` cookies of your local Firefox profiles into `cookies.json` |
| `--check-session` | report what the current cookies give you (and what is missing) and exit |
| `--render` | force the browser for listing pages even if HTTP finds links |
| `--login` | log in with Chromium, then write `urls.txt` + `cookies.json` |
| `--login-http` | log in with plain HTTP requests only (no browser) and write `cookies.json` |
| `--parse-form <URL>` | show the login form a page serves (GET only, no credentials sent) |
| `--dump-network` | with `--dump-url`: log the requests the page makes (how the API was found) |
| `--merkliste-url <URL>` | default `https://www.spiegel.de/fuermich/merkliste` |
| `--user` / `--password` | credentials (defaults: `$SPIEGEL_USER`, `$SPIEGEL_PASS`) |
| `--headed` | show the browser window (debug a login) |
| `--chrome <PATH>` | browser binary to drive |
| `--cookies <PATH>` | session cookies, default `cookies.json` |
| `--cookie name=value` | extra cookie (login-free escape hatch) |
| `--proxy <URL>` | proxy for HTTP and the browser (`$SPIEGEL_PROXY`) |
| `-o, --out <DIR>` | output directory, default `articles` |
| `-j, --jobs <N>` | parallel articles, default 3 |
| `--delay-ms <MS>` | politeness delay between requests, default 300 |
| `--no-images`, `--no-audio`, `--force` | skip / re-download media |
| `-n, --limit <N>` | stop after N articles |
| `--dry-run` | resolve and print the work list only |
| `-v, --verbose` | per-file logging |
| `--dump-url <URL> --eval <JS>` / `--js <JS>` / `--click-text <T>` | debug: render a page, run JS, print the result (used to develop the selectors) |

## Sessions

* `--from-firefox` is the quickest route: it copies `cookies.sqlite` of every
  Firefox profile (a running Firefox keeps a lock on the file, so the database
  is read from a temporary copy), keeps the non-expired `*.spiegel.de` rows and
  merges them into `cookies.json`. `--check-session` then tells you whether the
  set contains a live session — the give-away cookies are `sara_user_session`,
  `accessInfo`, `userInfo`, `authId` (`accessInfo` is a JWT whose `access`
  object lists your entitlements, e.g. `"Spplus": true`).
* `--login-http` does the same without a browser: it GETs the form, POSTs the
  e-mail (`loginform:username` + `_csrf` + `jakarta.faces.ViewState` +
  `loginform:submit`), then POSTs the password in the step the server reveals,
  and only accepts the result if a session cookie comes back.
* `--login` drives the SSO form itself: it asks for the e-mail address (visible)
  and the password (**without echo**), fills
  `gruppenkonto.spiegel.de/anmelden.html`, submits and waits for the session
  cookie. Credentials come from `--user`/`--password`, else `$SPIEGEL_USER`/
  `$SPIEGEL_PASS`, else the prompt; nothing else in the program ever reads them.
  It writes `cookies.json` plus `urls.txt` — the latter only when the login was
  confirmed, so a failed attempt never leaves you with an anonymous link list.
  Both form variants are handled (e-mail + password on one page, or e-mail →
  submit → password). On failure it prints what the page looked like (captcha,
  2FA field, SSO error text); `--headed` shows the window. Later runs pick it up automatically; the browser is not needed
  again until the session expires.
* Already have a session? Export the cookies from your browser into
  `cookies.json` (`{"cookies":[{"name":…,"value":…,"domain":…,"path":…}]}`) and
  skip `--login`, or pass `--cookie 'SPGR-User=…'`.
* Manual-login fallback: run with `--list-url … --render --headed`, log in by
  hand in the window; the cookies are saved to `cookies.json` for the HTTP crawl.

## Honest status

**Verified** against the live site:

* crawling `https://www.spiegel.de/fuermich/merkliste` ("Ihre Artikel"): 5
  bookmarks → 5 folders, complete SPIEGEL+ text (up to 2314 words), 26
  images/audio files, 45 MB, 0 failures
* the Merkliste listing **without any browser**: two JSON requests return the
  same five bookmarks (with titles and access level); without a session the
  bookmarks endpoint answers `400 http: named cookie not present`
* the login form is parsed correctly from the live page (`--parse-form` prints
  action, method, `loginform:username`, `password`, `loginform:submit` and the
  six hidden fields)
* article extraction, markdown + JSON output, resumable and parallel downloads
* section/feed listing over plain HTTP (`/politik/index.rss`, `/politik/`)
* the browser layer: Chromium launch, navigation, scrolling, JS evaluation,
  link extraction, `Network.setCookies` / `Network.getCookies`

**Not verified end-to-end**: the credential login, neither `--login` (browser)
nor `--login-http` (plain requests), because it needs a real account and I will
not send a fake one to their SSO. For the plain path there is direct evidence
that it is replayable — `POST` (not GET), `application/x-www-form-urlencoded`,
`jakarta.faces.ViewState=stateless`, no captcha/hCaptcha/Turnstile, no Akamai
sensor, no DataDome/Kasada/PerimeterX, and the two-step shape
(`loginform:step=anmelden`, password field `display:none` in step 1) matches the
recorded browser session. Use `-v` to watch the two POSTs and `--parse-form` to
inspect the form at any time; nothing is logged but field names.

What *is* verified
around it, on the live form: the e-mail field and the password field are located
by their `<label>` text (`email_field: true`, `password_field: true`), the
submit button is "Anmelden oder Konto erstellen", the prompt appears on a
terminal and refuses an empty password before anything is submitted, and success
is decided by the server setting a session cookie (`sara_user_session`,
`accessInfo`, …) — not by a page flag. If the submit itself trips on a captcha
or 2FA, `--login --headed` shows the window and the failure prints the page
state.

Two assumptions that measurement killed, recorded so they are not repeated:

* `app.pageContext.isBookmarkEnabled` is **not** a login indicator — it is true
  on article pages and false on the Merkliste page, regardless of the session.
* `isAccessibleForFree: false` in JSON-LD is static metadata ("this article is
  normally paid"), not evidence of truncation: with a SPIEGEL+ session the text
  is complete. Only the server's own `paywall.attributes.is_active` or the
  truncation markers set the flag now.

## Behave

Authenticated personal use only: the Merkliste and paywalled articles are yours
to read, not to redistribute. Keep `--delay-ms` (and `-j`) polite, and delete
`cookies.json`/`urls.txt` when you are done — they contain live session cookies.

⚠️ **`script.py` in this directory still contains your e-mail address and
password in plain text** (and that password was pasted into this chat). Rotate
it, then delete the file — the Rust crawler replaces it.
