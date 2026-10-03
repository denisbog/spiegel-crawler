# spiegel-crawler

Downloads SPIEGEL articles — **text, images and audio** — from a URL list, an RSS
feed, a section page or your personal Merkliste. Pure Rust: no Python, no
Playwright, no WebDriver.

```bash
cargo build --release

# feeds / section pages need no session at all
./target/release/spiegel-crawler --list-url https://www.spiegel.de/schlagzeilen/index.rss -n 5 -o articles

# your own list ("Ihre Artikel" = /fuermich/merkliste): take the session you
# already have in Firefox, then crawl it
./target/release/spiegel-crawler --from-firefox          # spiegel.de cookies -> cookies.json
./target/release/spiegel-crawler --check-session          # tells you what is missing, if anything
./target/release/spiegel-crawler --list-url https://www.spiegel.de/fuermich/merkliste \
    --render -o articles-ihre-artikel -j 2
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
| Listing (Merkliste) | headless Chromium over CDP, with the session cookies installed via `Network.setCookies` | `spiegel.de/fuermich/merkliste` has **no** article links in its HTML — the page fetches the list with JS, so a plain HTTP GET sees an empty list even when you are logged in |
| Session | `--from-firefox` reads `cookies.sqlite` (pure Rust, via `rusqlite`) | if you are logged in in your own browser you already have a session — no need to automate a login at all |
| Login (optional) | headless Chromium over CDP | the SSO form is a JSF app on `gruppenkonto.spiegel.de` with captcha/2FA risk; `--login` is there for machines without a browser session |
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
* `--login` drives the SSO form itself and writes `cookies.json` plus
  `urls.txt` — the latter only when the login was confirmed (a session cookie
  appeared), so a failed attempt never leaves you with an anonymous link list. Later runs pick it up automatically; the browser is not needed
  again until the session expires.
* Already have a session? Export the cookies from your browser into
  `cookies.json` (`{"cookies":[{"name":…,"value":…,"domain":…,"path":…}]}`) and
  skip `--login`, or pass `--cookie 'SPGR-User=…'`.
* Manual-login fallback: run with `--list-url … --render --headed`, log in by
  hand in the window; the cookies are saved to `cookies.json` for the HTTP crawl.

## Honest status

**Verified** against the live site:

* crawling `https://www.spiegel.de/fuermich/merkliste` ("Ihre Artikel") with a
  Firefox session: 5 bookmarks → 5 folders, complete SPIEGEL+ text (up to 2314
  words), 26 images/audio files, 45 MB, 0 failures
* article extraction, markdown + JSON output, resumable and parallel downloads
* section/feed listing over plain HTTP (`/politik/index.rss`, `/politik/`)
* the browser layer: Chromium launch, navigation, scrolling, JS evaluation,
  link extraction, `Network.setCookies` / `Network.getCookies`

**Not verified**: the `--login` credential flow (it needs a real account, and the
test account was withdrawn). The form handling is verified up to the submit —
the form is found, both fields are located by their `<label>` text and filled
("Anmelden oder Konto erstellen" is the submit button) — and success is now
decided by a session cookie appearing, not by a page flag.

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
