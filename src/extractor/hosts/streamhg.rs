use async_trait::async_trait;
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

use crate::extractor::base::{
    BaseExtractor, ExtraParams, Extractor, ExtractorError, ExtractorResult,
};
use crate::extractor::packed::{is_packed, unpack_packed_js};

fn hls2_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#""hls2":"([^"]+)""#).unwrap())
}

pub struct StreamHGExtractor(pub BaseExtractor);

impl StreamHGExtractor {
    pub fn new(request_headers: HashMap<String, String>, proxy_url: Option<String>) -> Self {
        Self(BaseExtractor::new(request_headers, proxy_url))
    }

    fn find_hls2(html: &str) -> Option<String> {
        hls2_re()
            .captures(html)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().replace("\\/", "/"))
    }
}

#[async_trait]
impl Extractor for StreamHGExtractor {
    fn host_name(&self) -> &'static str {
        "StreamHG"
    }

    async fn extract(
        &self,
        url: &str,
        _extra: &ExtraParams,
    ) -> Result<ExtractorResult, ExtractorError> {
        let headers: HashMap<String, String> = HashMap::new();

        let (html, _) = self.0.get_text(url, Some(headers)).await?;

        // Try direct match first; fall back to unpacking packed JS.
        let final_url = if let Some(u) = Self::find_hls2(&html) {
            u
        } else if is_packed(&html) {
            let unpacked = unpack_packed_js(&html)
                .ok_or_else(|| ExtractorError::extract("StreamHG: failed to unpack JS"))?;
            Self::find_hls2(&unpacked)
                .ok_or_else(|| ExtractorError::extract("StreamHG: hls2 URL not found after unpack"))?
        } else {
            return Err(ExtractorError::extract("StreamHG: hls2 URL not found"));
        };

        Ok(ExtractorResult {
            destination_url: final_url,
            request_headers: self.0.base_headers.clone(),
            mediaflow_endpoint: "hls_manifest_proxy",
        })
    }
}
