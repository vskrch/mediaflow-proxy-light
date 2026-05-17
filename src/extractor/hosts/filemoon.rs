use async_trait::async_trait;
use std::collections::HashMap;

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

use crate::extractor::base::{
    BaseExtractor, ExtraParams, Extractor, ExtractorError, ExtractorResult,
};

pub struct FileMoonExtractor(pub BaseExtractor);

impl FileMoonExtractor {
    pub fn new(request_headers: HashMap<String, String>, proxy_url: Option<String>) -> Self {
        Self(BaseExtractor::new(request_headers, proxy_url))
    }
}

/// Decode a base64url-encoded string (with or without padding) into bytes.
fn base64url_decode(input: &str) -> Result<Vec<u8>, ExtractorError> {
    // Normalize: replace URL-safe chars with standard base64 chars, strip padding.
    let normalized = input.replace('-', "+").replace('_', "/");
    // Strip any existing padding then re-decode without pad (engine handles it).
    let stripped = normalized.trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(stripped)
        .map_err(|e| ExtractorError::extract(format!("base64url decode failed: {e}")))
}

/// Concatenate all key_parts (each base64url-decoded) into a single byte vector.
fn combine_key_parts(key_parts: &[serde_json::Value]) -> Result<Vec<u8>, ExtractorError> {
    let mut combined = Vec::new();
    for part in key_parts {
        let s = part
            .as_str()
            .ok_or_else(|| ExtractorError::extract("key_part is not a string"))?;
        combined.extend(base64url_decode(s)?);
    }
    Ok(combined)
}

/// Decrypt the `playback` object using AES-256-GCM.
///
/// The `aes-gcm` crate's `decrypt` method expects the ciphertext and the
/// 16-byte authentication tag to be **concatenated** (ciphertext || tag),
/// which is exactly the layout FileMoon uses (tag is the last 16 bytes of
/// the payload).  So we pass `payload` directly.
fn decrypt_playback(playback: &serde_json::Value) -> Result<serde_json::Value, ExtractorError> {
    let key_parts = playback["key_parts"]
        .as_array()
        .ok_or_else(|| ExtractorError::extract("playback.key_parts missing or not an array"))?;

    let key_bytes = combine_key_parts(key_parts)?;

    let iv_str = playback["iv"]
        .as_str()
        .ok_or_else(|| ExtractorError::extract("playback.iv missing"))?;
    let iv_bytes = base64url_decode(iv_str)?;

    let payload_str = playback["payload"]
        .as_str()
        .ok_or_else(|| ExtractorError::extract("playback.payload missing"))?;
    let payload_bytes = base64url_decode(payload_str)?;

    // Build cipher — key must be exactly 32 bytes for AES-256.
    let cipher = Aes256Gcm::new_from_slice(&key_bytes)
        .map_err(|e| ExtractorError::extract(format!("AES-256-GCM key init failed: {e}")))?;

    // Nonce must be exactly 12 bytes for GCM.
    if iv_bytes.len() != 12 {
        return Err(ExtractorError::extract(format!(
            "unexpected IV length: {} (expected 12)",
            iv_bytes.len()
        )));
    }
    let nonce = Nonce::from_slice(&iv_bytes);

    // `aes-gcm` decrypt expects ciphertext || tag (last 16 bytes = tag).
    let plaintext = cipher
        .decrypt(nonce, payload_bytes.as_ref())
        .map_err(|_| ExtractorError::extract("AES-256-GCM decryption failed"))?;

    let json: serde_json::Value = serde_json::from_slice(&plaintext).map_err(|e| {
        ExtractorError::extract(format!("JSON parse of decrypted payload failed: {e}"))
    })?;

    Ok(json)
}

#[async_trait]
impl Extractor for FileMoonExtractor {
    fn host_name(&self) -> &'static str {
        "FileMoon"
    }

    async fn extract(
        &self,
        url: &str,
        _extra: &ExtraParams,
    ) -> Result<ExtractorResult, ExtractorError> {
        // ----------------------------------------------------------------
        // 1. Parse the video code from the URL path.
        //    Expected formats: https://filemoon.sx/e/{code}
        //                      https://filemoon.sx/d/{code}
        // ----------------------------------------------------------------
        let parsed = url::Url::parse(url)
            .map_err(|e| ExtractorError::extract(format!("invalid URL: {e}")))?;

        let path = parsed.path().trim_end_matches('/');
        let code = path
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty() && *s != "e" && *s != "d")
            .ok_or_else(|| {
                ExtractorError::extract(format!("could not extract video code from URL: {url}"))
            })?;

        // ----------------------------------------------------------------
        // 2. Call the API endpoint.
        // ----------------------------------------------------------------
        let scheme = parsed.scheme();
        let host = parsed
            .host_str()
            .ok_or_else(|| ExtractorError::extract("URL has no host"))?;
        let api_url = format!("{scheme}://{host}/api/videos/{code}");

        let mut req_headers = HashMap::new();
        req_headers.insert("referer".to_string(), url.to_string());

        let (text, _final_url) = self.0.get_text(&api_url, Some(req_headers)).await?;

        // ----------------------------------------------------------------
        // 3. Parse the JSON response.
        // ----------------------------------------------------------------
        let data: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| ExtractorError::extract(format!("FileMoon API JSON parse failed: {e}")))?;

        if let Some(err_msg) = data.get("error") {
            return Err(ExtractorError::extract(format!(
                "FileMoon API error: {err_msg}"
            )));
        }

        let playback = data.get("playback").ok_or_else(|| {
            ExtractorError::extract("FileMoon: no playback field in API response")
        })?;

        if playback.get("key_parts").is_none() || playback.get("payload").is_none() {
            return Err(ExtractorError::extract(
                "FileMoon: playback data is incomplete (missing key_parts or payload)",
            ));
        }

        // ----------------------------------------------------------------
        // 4. Decrypt the playback object.
        // ----------------------------------------------------------------
        let decrypted = decrypt_playback(playback)?;

        // ----------------------------------------------------------------
        // 5. Find the HLS source.
        // ----------------------------------------------------------------
        let sources = decrypted["sources"]
            .as_array()
            .ok_or_else(|| ExtractorError::extract("FileMoon: decrypted payload has no sources"))?;

        let hls_source = sources
            .iter()
            .find(|s| s["mime_type"].as_str() == Some("application/vnd.apple.mpegurl"))
            .ok_or_else(|| {
                ExtractorError::extract("FileMoon: no HLS source found in decrypted playback")
            })?;

        let destination_url = hls_source["url"]
            .as_str()
            .ok_or_else(|| ExtractorError::extract("FileMoon: HLS source has no url field"))?
            .to_string();

        // ----------------------------------------------------------------
        // 6. Build result headers and return.
        // ----------------------------------------------------------------
        let mut result_headers = self.0.base_headers.clone();
        result_headers.insert("referer".to_string(), url.to_string());

        Ok(ExtractorResult {
            destination_url,
            request_headers: result_headers,
            mediaflow_endpoint: "hls_manifest_proxy",
        })
    }
}
