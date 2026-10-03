//! HTTP layer: one shared client (with the optional login cookie jar),
//! polite pacing, retries, and resumable media downloads.

use anyhow::{bail, Context, Result};
use reqwest::blocking::{Client, Response};
use reqwest::cookie::Jar;
use reqwest::header::{ACCEPT, ACCEPT_ENCODING, ACCEPT_LANGUAGE, RANGE};
use reqwest::StatusCode;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

pub struct Fetcher {
    client: Client,
    delay: Duration,
    retries: u32,
    pub verbose: bool,
}

impl Fetcher {
    pub fn new(jar: Option<Arc<Jar>>, delay_ms: u64, verbose: bool, proxy: Option<&str>) -> Result<Self> {
        let mut b = Client::builder()
            .user_agent(UA)
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(15))
            .pool_max_idle_per_host(8);
        if let Some(jar) = jar {
            b = b.cookie_provider(jar);
        }
        if let Some(p) = proxy {
            b = b.proxy(reqwest::Proxy::all(p).with_context(|| format!("bad --proxy {p:?}"))?);
        }
        Ok(Self {
            client: b.build().context("building HTTP client")?,
            delay: Duration::from_millis(delay_ms),
            retries: 3,
            verbose,
        })
    }

    fn pace(&self) {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
    }

    fn get(&self, url: &str, identity_encoding: bool) -> Result<Response> {
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..self.retries {
            if attempt > 0 {
                let backoff = Duration::from_millis(800 * (1 << attempt));
                if self.verbose {
                    eprintln!("  retry {attempt} for {url} after {backoff:?}");
                }
                std::thread::sleep(backoff);
            }
            self.pace();
            let mut req = self
                .client
                .get(url)
                .header(ACCEPT_LANGUAGE, "de-DE,de;q=0.9,en;q=0.8");
            req = if identity_encoding {
                // Needed for byte-range resumption: no transparent decompression.
                req.header(ACCEPT_ENCODING, "identity")
            } else {
                req.header(
                    ACCEPT,
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                )
            };
            match req.send() {
                Ok(resp) => {
                    let code = resp.status();
                    if code.is_success() {
                        return Ok(resp);
                    }
                    if code == StatusCode::TOO_MANY_REQUESTS || code.is_server_error() {
                        last = Some(anyhow::anyhow!("HTTP {code}"));
                        continue;
                    }
                    bail!("HTTP {code} for {url}");
                }
                Err(e) => last = Some(anyhow::Error::from(e)),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("request failed")))
            .with_context(|| format!("GET {url}"))
    }

    /// Fetch a page, returning `(final_url, html)`.
    pub fn text(&self, url: &str) -> Result<(String, String)> {
        let resp = self.get(url, false)?;
        let final_url = resp.url().to_string();
        let body = resp.text().context("decoding response body")?;
        Ok((final_url, body))
    }

    /// Download to `dest`, resuming a `dest.part` file if present.
    /// Returns the number of bytes on disk (0 for a skipped existing file).
    pub fn download(&self, url: &str, dest: &Path, force: bool) -> Result<u64> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        if !force {
            if let Ok(m) = std::fs::metadata(dest) {
                if m.len() > 0 {
                    return Ok(0);
                }
            }
        }
        let part: PathBuf = dest.with_extension(format!(
            "{}part",
            dest.extension().and_then(|e| e.to_str()).map(|e| format!("{e}.")).unwrap_or_default()
        ));
        let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);

        let mut req = self
            .client
            .get(url)
            .header(ACCEPT_ENCODING, "identity")
            .header(ACCEPT, "*/*");
        if have > 0 {
            req = req.header(RANGE, format!("bytes={have}-"));
        }
        let resp = req.send().with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let resumed = status == StatusCode::PARTIAL_CONTENT;
        if resumed && self.verbose {
            eprintln!("  resuming {} at {} bytes", part.display(), have);
        }

        let mut file = if resumed {
            OpenOptions::new().append(true).open(&part)?
        } else {
            File::create(&part)?
        };
        let mut resp = resp;
        let mut buf = vec![0u8; 64 * 1024];
        let mut written = 0u64;
        loop {
            let n = resp.read(&mut buf)?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            written += n as u64;
        }
        file.flush()?;
        drop(file);
        std::fs::rename(&part, dest)
            .with_context(|| format!("renaming {} -> {}", part.display(), dest.display()))?;
        Ok(written)
    }
}
