use async_trait::async_trait;
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

use crate::extractor::base::{
    BaseExtractor, ExtraParams, Extractor, ExtractorError, ExtractorResult,
};

fn embed_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"/embed-([a-zA-Z0-9]+)\.html").unwrap())
}

fn sources_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"sources\s*:\s*\[\s*\{\s*file\s*:\s*['"]([^'"]+)"#).unwrap())
}

pub struct VidmolyExtractor(pub BaseExtractor);

impl VidmolyExtractor {
    pub fn new(request_headers: HashMap<String, String>, proxy_url: Option<String>) -> Self {
        Self(BaseExtractor::new(request_headers, proxy_url))
    }
}

#[async_trait]
impl Extractor for VidmolyExtractor {
    fn host_name(&self) -> &'static str {
        "Vidmoly"
    }

    async fn extract(
        &self,
        url: &str,
        _extra: &ExtraParams,
    ) -> Result<ExtractorResult, ExtractorError> {
        let embed_id = embed_id_re()
            .captures(url)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .ok_or_else(|| ExtractorError::extract("Vidmoly: embed ID not found in URL"))?;

        let mut headers = HashMap::new();
        headers.insert(
            "user-agent".to_string(),
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/120 Safari/537.36"
                .to_string(),
        );
        headers.insert(
            "accept".to_string(),
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8"
                .to_string(),
        );
        headers.insert("accept-language".to_string(), "en-US,en;q=0.5".to_string());
        headers.insert("connection".to_string(), "keep-alive".to_string());
        headers.insert("referer".to_string(), url.to_string());
        headers.insert(
            "cookie".to_string(),
            format!("cf_turnstile_demo_pass_{embed_id}=1"),
        );
        headers.insert("sec-fetch-dest".to_string(), "document".to_string());
        headers.insert("sec-fetch-mode".to_string(), "navigate".to_string());
        headers.insert("sec-fetch-site".to_string(), "same-origin".to_string());

        let (html, _) = self.0.get_text(url, Some(headers.clone())).await?;

        let master_url = sources_re()
            .captures(&html)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .ok_or_else(|| ExtractorError::extract("Vidmoly: stream URL not found"))?;

        if !master_url.starts_with("http") {
            return Err(ExtractorError::extract(format!(
                "Vidmoly: unexpected stream URL: {}",
                &master_url[..master_url.len().min(120)]
            )));
        }

        Ok(ExtractorResult {
            destination_url: master_url,
            request_headers: headers,
            mediaflow_endpoint: "hls_manifest_proxy",
        })
    }
}
