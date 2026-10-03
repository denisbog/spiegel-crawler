# spiegel-crawler

Downloads SPIEGEL articles — **text, images and audio** — from a URL list, an RSS
feed, a section page or your personal Merkliste ("Ihre Artikel"). Pure Rust, plain
HTTP requests: no browser, no Playwright, no WebDriver.

Two entry points, everything else follows from them:

```bash
cargo build --release

# 1. log in once (prompts for e-mail, then the password without echo)
./target/release/spiegel-crawler --login-http

# 2. crawl a list
./target/release/spiegel-crawler --list-url https://www.spiegel.de/fuermich/merkliste -o articles
./target/release/spiegel-crawler --list-url https://www.spiegel.de/politik/index.rss -o articles
./target/release/spiegel-crawler --check-session          # is the session still good?
```

## What you get

```
articles/2026-09-29_berichte-ueber-warnung-vor-hamas-angriff-…/
├── article.md      title, lead, body, pull quotes, inline images, audio link
├── article.json    the same as structured data (blocks, images[], audio, flags)
├── images/01_a7322d87-…_w1920.webp
└── audio/berichte-ueber-warnung-vor-hamas-angriff-…​.mp3
```

Re-running is cheap: existing non-empty media files are skipped, interrupted
downloads resume (`*.part` + HTTP range).

## How the two paths work

**`--login-http`** (`src/login.rs`, `src/form.rs`)

The SSO page `gruppenkonto.spiegel.de/anmelden.html` is a plain
`POST application/x-www-form-urlencoded` JSF form with
`jakarta.faces.ViewState=stateless`, no captcha and no bot-protection script, so
it is replayed directly:

1. `GET` the form → keep `loginform`, `_csrf`, `targetUrl`,
   `requestAccessToken`, `loginform:step`, `jakarta.faces.ViewState`
2. `POST` the e-mail (`loginform:username` + those hidden fields +
   `loginform:submit`); the page hides the password field in this step
   (`style="display:none"`), so step 3 is only needed if the response reveals it
3. `POST` the password
4. accept the result **only** if a session cookie came back (`accessInfo`,
   `sara_user_session`, … `accessInfo` is a JWT whose `access` object lists your
   entitlements, e.g. `"Spplus": true`)

Credentials come from `--user`/`--password`, else `$SPIEGEL_USER`/`$SPIEGEL_PASS`,
else the prompt (password without echo). The session is written to
`cookies.json`, which is the only thing later runs need.

**`--list-url`**

| Page | Path |
|---|---|
| `/fuermich/…` (your own lists) | two JSON requests: `GET /services/depot/api/v1/bookmarks` → ids, then `GET /services/sitesearch/fetch?ids=…` → `{url, title, access_level}` (`src/api.rs`). The page itself has no article links in its HTML — it renders them with JavaScript, which is why scraping it does not work; without a session the API answers `400 http: named cookie not present`. |
| RSS feeds, section pages | the article links are in the HTML, scoped to `<main>` so header/footer links are not mistaken for content (`src/article.rs`) |

## Article extraction

| Data | Source |
|---|---|
| headline, authors, dates, section, tags | `<script type="application/ld+json">` (schema.org `NewsArticle`) |
| lead image (largest variant) | `image`/`thumbnailUrl` of that JSON-LD, plus `og:image` |
| document id, language, canonical URL | `<script type="application/settings+json">` → `app.pageContext`, `page.info` |
| **audio** | the same settings JSON → `app.pageContext.clip.audioUrl` / `downloadUrl` / `duration` (CDN: Omny/Triton Digital), with `<audio src>` and a `.mp3` regex as fallbacks |
| body text, headings, quotes, lists | the rendered `<article>`: `data-area="text"`, `"body"`, `"quote"`, `"intro"` |
| body images | `srcset`/`data-srcset` and `<picture><source>`; the widest candidate wins, duplicates collapse by image UUID |
| paywall state | `paywall.attributes.is_active` or the truncation markers — never the static `isAccessibleForFree` metadata, which is `false` for every SPIEGEL+ article even when you may read it in full |

Ads, "related articles", gift boxes (`data-component="gift-box"`) and navigation
are filtered out (`SKIP_AREAS`/`SKIP_CLASSES`/`SKIP_COMPONENTS` in `src/article.rs`).

## Options

| Flag | Meaning |
|---|---|
| `--login-http` | log in with plain HTTP requests, write `cookies.json` |
| `--user`, `--password` | credentials (defaults `$SPIEGEL_USER`, `$SPIEGEL_PASS`) |
| `-l, --list-url <URL>` | crawl every article of this page (repeatable) |
| `-u, --url <URL>` | a single article (repeatable) |
| `-f, --urls-file <PATH>` | one URL per line, `#` comments |
| `--cookies <PATH>` | session file, default `cookies.json` |
| `--cookie name=value` | extra cookie, a manual escape hatch |
| `--check-session` | report session cookies + bookmark count, then exit |
| `--proxy <URL>` | proxy for all requests (`$SPIEGEL_PROXY`) |
| `-o, --out <DIR>` | output directory, default `articles` |
| `-j, --jobs <N>` | parallel articles, default 3 |
| `--delay-ms <MS>` | politeness delay, default 300 |
| `-n, --limit <N>` | stop after N articles |
| `--no-images`, `--no-audio`, `--force` | skip / re-download media |
| `--dry-run` | resolve and print the work list only |
| `-v, --verbose` | per-file logging, login steps by field name |

## Status

Verified against the live site: `--login-http` produces a working session
(`--check-session` → `session cookies: accessInfo, userInfo`, `merkliste: 5
bookmark(s)`), the bookmark API returns the list, and crawling fetches complete
articles with text, images and audio (the five Merkliste bookmarks: up to 2314
words each, 21 images, 5 audio files, 45 MB, 0 failures). The form parser has
unit tests (`cargo test`), and RSS/section listing needs no session at all.

Not covered: 2FA or a captcha, should SPIEGEL ever add one to the SSO form — the
login would then report the page state instead of guessing. The internal
`/services/…` endpoints are undocumented plumbing; if they change, `--list-url`
on such a page will say it found nothing rather than silently crawl the wrong
links.

## Behave

Authenticated personal use only: your Merkliste and SPIEGEL+ articles are yours
to read, not to redistribute. Keep `--delay-ms` (and `-j`) polite, and delete
`cookies.json` once you are done — it is a live session.

`cookies.json`, `urls.txt`, `script.py`, `articles*/` and `test/` are gitignored:
sessions, credentials and downloaded articles do not belong in the repository.
