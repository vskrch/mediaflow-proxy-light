use async_trait::async_trait;
use regex::Regex;
use rquest::header::{HeaderMap, HeaderName, HeaderValue};
use std::collections::HashMap;
use std::sync::OnceLock;

use crate::extractor::base::{
    build_chrome_client, BaseExtractor, ExtraParams, Extractor, ExtractorError, ExtractorResult,
};

// ---------------------------------------------------------------------------
// Compiled regex helpers
// ---------------------------------------------------------------------------

fn msf_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"/msf/").unwrap())
}

fn strip_comments_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<!--.*?-->").unwrap())
}

fn strip_hidden_divs_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?si)<div[^>]*style=["'][^"']*display\s*:\s*none[^"']*["'][^>]*>.*?</div>"#)
            .unwrap()
    })
}

fn buttok_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)href=["'](https?://[^"']+)["'][^>]*>\s*<button[^>]*id=["']buttok["']"#)
            .unwrap()
    })
}

fn continue_btn_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"href=["'](https?://[^"']+)["'][^>]*>\s*<button[^>]*>\s*[Cc]\s*[Oo]\s*[Nn]\s*[Tt]\s*[Ii]\s*[Nn]\s*[Uu]\s*[Ee]"#,
        )
        .unwrap()
    })
}

fn uprots_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)href=["'](https?://[^"']*uprot(?:s|em)/[^"']+)["']"#).unwrap()
    })
}

fn stayonline_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"https?://(?:www\.)?(?:stayonline\.pro|maxstream\.video)[^"'\s<>\\]+"#)
            .unwrap()
    })
}

fn window_location_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"window\.location(?:\.href)?\s*=\s*["']([^"']+)["']"#).unwrap())
}

fn meta_refresh_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)content=["']0;\s*url=([^"']+)["']"#).unwrap())
}

fn direct_sources_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"sources:\s*\[\{src:\s*"([^"]+)""#).unwrap())
}

fn packed_js_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\}\('(.+)',.+,'(.+)'\.split").unwrap())
}

// ---------------------------------------------------------------------------
// Honeypot stripping helpers
// ---------------------------------------------------------------------------

/// Remove HTML comments and `display:none` divs so we don't pick up honeypot URLs.
fn strip_uprot_honeypots(html: &str) -> String {
    let no_comments = strip_comments_re().replace_all(html, "");
    let no_hidden = strip_hidden_divs_re().replace_all(&no_comments, "");
    no_hidden.into_owned()
}

/// Parse a (possibly uprot/stayonline) HTML page and return the next URL to follow.
/// Implements the 5-step strategy from the Python reference implementation.
fn parse_uprot_html(html: &str) -> Option<String> {
    let cleaned = strip_uprot_honeypots(html).replace("\\/", "/");

    // 1. id="buttok" CONTINUE button
    if let Some(cap) = buttok_re().captures(&cleaned) {
        if let Some(m) = cap.get(1) {
            return Some(m.as_str().to_string());
        }
    }

    // 2. Generic <a><button>Continue</button></a>
    if let Some(cap) = continue_btn_re().captures(&cleaned) {
        if let Some(m) = cap.get(1) {
            return Some(m.as_str().to_string());
        }
    }

    // 3. Unique uprots/uprotem URL (appears exactly once — not a honeypot duplicate)
    let all_uprots: Vec<String> = uprots_re()
        .captures_iter(&cleaned)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
        .collect();
    if !all_uprots.is_empty() {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for u in &all_uprots {
            *counts.entry(u.clone()).or_insert(0) += 1;
        }
        let unique: Vec<String> = counts
            .into_iter()
            .filter(|(_, c)| *c == 1)
            .map(|(u, _)| u)
            .collect();
        if let Some(u) = unique.into_iter().next() {
            return Some(u);
        }
    }

    // 4. Generic stayonline / maxstream URL (exclude honeypot placeholder)
    if let Some(m) = stayonline_re().find(&cleaned) {
        let found = m.as_str().to_string();
        if !found.contains("/uprots/123456789012") {
            return Some(found);
        }
    }

    // 5. window.location / meta refresh redirect
    if let Some(cap) = window_location_re().captures(&cleaned) {
        if let Some(m) = cap.get(1) {
            return Some(m.as_str().to_string());
        }
    }
    if let Some(cap) = meta_refresh_re().captures(&cleaned) {
        if let Some(m) = cap.get(1) {
            return Some(m.as_str().to_string());
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Extractor struct
// ---------------------------------------------------------------------------

pub struct MaxstreamExtractor {
    pub base: BaseExtractor,
    chrome_client: rquest::Client,
}

impl MaxstreamExtractor {
    pub fn new(request_headers: HashMap<String, String>, proxy_url: Option<String>) -> Self {
        let chrome_client = build_chrome_client(proxy_url.as_deref());
        Self {
            base: BaseExtractor::new(request_headers, proxy_url),
            chrome_client,
        }
    }

    /// Fetch a URL with the Chrome-impersonating client and return (body, final_url).
    async fn chrome_get(&self, url: &str) -> Result<(String, String), ExtractorError> {
        let resp = self
            .chrome_client
            .get(url)
            .headers(HeaderMap::new())
            .send()
            .await
            .map_err(|e| ExtractorError::Network(e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            return Err(ExtractorError::Http {
                status,
                message: format!("HTTP {status} from {url}"),
            });
        }

        let final_url = resp.url().to_string();
        let text = resp
            .text()
            .await
            .map_err(|e| ExtractorError::Network(e.to_string()))?;
        Ok((text, final_url))
    }

    /// Follow the uprots/stayonline redirect chain (up to 10 hops) until we reach a
    /// `maxstream.video/emvvv/` URL or a `maxsun*.online` URL, then normalise the path.
    async fn follow_uprots_chain(&self, start: &str) -> Result<String, ExtractorError> {
        let mut current = start.to_string();

        for _ in 0..10 {
            // Terminal conditions
            let host = url_host(&current);
            if host.contains("maxstream.video") && current.contains("/emvvv/") {
                break;
            }
            if host.contains("maxsun") && host.ends_with(".online") {
                current = current.replace("/watchfree/", "/emvvv/");
                break;
            }

            let (text, _) = self.chrome_get(&current).await?;
            let next = match parse_uprot_html(&text) {
                Some(u) => u,
                None => break,
            };
            if next == current {
                break;
            }
            current = next;
        }

        Ok(current)
    }

    /// Extract the final HLS URL from packed JS on the maxstream player page.
    fn unpack_player_js(html: &str) -> Result<String, ExtractorError> {
        let cap = packed_js_re()
            .captures(html)
            .ok_or_else(|| ExtractorError::extract("Maxstream: packed JS not found"))?;

        let s1 = &cap[2];
        let terms: Vec<&str> = s1.split('|').collect();

        let urlset_idx = terms
            .iter()
            .position(|&t| t == "urlset")
            .ok_or_else(|| ExtractorError::extract("Maxstream: urlset not found"))?;
        let hls_idx = terms
            .iter()
            .position(|&t| t == "hls")
            .ok_or_else(|| ExtractorError::extract("Maxstream: hls not found"))?;
        let sources_idx = terms
            .iter()
            .position(|&t| t == "sources")
            .ok_or_else(|| ExtractorError::extract("Maxstream: sources not found"))?;

        let reversed_elements: Vec<&str> = terms[urlset_idx + 1..hls_idx]
            .iter()
            .rev()
            .cloned()
            .collect();
        let first_part: Vec<&str> = terms[hls_idx + 1..sources_idx]
            .iter()
            .rev()
            .cloned()
            .collect();

        let mut first_url_part = String::new();
        for part in &first_part {
            if part.contains('0') {
                first_url_part.push_str(part);
            } else {
                first_url_part.push_str(part);
                first_url_part.push('-');
            }
        }

        let base = format!("https://{first_url_part}.host-cdn.net/hls/");
        let final_url = if reversed_elements.len() == 1 {
            format!("{base},{}.urlset/master.m3u8", reversed_elements[0])
        } else {
            let mut b = base.clone();
            for (i, el) in reversed_elements.iter().enumerate() {
                b.push_str(el);
                b.push(',');
                if i == reversed_elements.len() - 1 {
                    b.push_str(".urlset/master.m3u8");
                }
            }
            b
        };

        Ok(final_url)
    }
}

// ---------------------------------------------------------------------------
// Extractor trait impl
// ---------------------------------------------------------------------------

#[async_trait]
impl Extractor for MaxstreamExtractor {
    fn host_name(&self) -> &'static str {
        "Maxstream"
    }

    async fn extract(
        &self,
        url: &str,
        _extra: &ExtraParams,
    ) -> Result<ExtractorResult, ExtractorError> {
        // Fix 1: Only replace the path segment `/msf/` → `/mse/` (not global "msf"→"mse").
        let fixed_url = msf_re().replace(url, "/mse/").into_owned();

        // Fetch the initial uprot page (uses Chrome TLS fingerprint for bypass).
        let (html1, _) = self.chrome_get(&fixed_url).await?;

        // Parse the first page for the redirect URL.
        let first_hop = parse_uprot_html(&html1).ok_or_else(|| {
            ExtractorError::extract(
                "Maxstream: uprot redirect link not found (captcha may be required)",
            )
        })?;

        // Follow the full redirect chain to the final maxstream.video player URL.
        let maxstream_url = self.follow_uprots_chain(&first_hop).await?;

        // Fetch the player page.
        let mut extra_hdrs = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str("en-US,en;q=0.5") {
            extra_hdrs.insert(HeaderName::from_static("accept-language"), v);
        }
        let resp = self
            .chrome_client
            .get(&maxstream_url)
            .headers(extra_hdrs)
            .send()
            .await
            .map_err(|e| ExtractorError::Network(e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            return Err(ExtractorError::Http {
                status,
                message: format!("HTTP {status} from {maxstream_url}"),
            });
        }
        let html2 = resp
            .text()
            .await
            .map_err(|e| ExtractorError::Network(e.to_string()))?;

        let mut result_headers = self.base.base_headers.clone();
        result_headers.insert("referer".to_string(), maxstream_url.clone());

        // Fix 5: Try the direct `sources:[{src:"..."` pattern first.
        if let Some(cap) = direct_sources_re().captures(&html2) {
            if let Some(m) = cap.get(1) {
                return Ok(ExtractorResult {
                    destination_url: m.as_str().to_string(),
                    request_headers: result_headers,
                    mediaflow_endpoint: "hls_manifest_proxy",
                });
            }
        }

        // Fallback: decode the packed JS player.
        let final_url = Self::unpack_player_js(&html2)?;

        Ok(ExtractorResult {
            destination_url: final_url,
            request_headers: result_headers,
            mediaflow_endpoint: "hls_manifest_proxy",
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn url_host(url: &str) -> &str {
    url.trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("")
}
