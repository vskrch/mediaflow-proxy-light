/// HLS M3U8 manifest processor.
///
/// Fetches an upstream M3U8 playlist, rewrites all segment/playlist/key URLs to
/// go through the local proxy, and returns the modified content.
///
/// Port of Python `M3U8Processor` in `mediaflow_proxy/utils/m3u8_processor.py`.
use std::collections::HashMap;

use m3u8_rs::{MasterPlaylist, MediaPlaylist, MediaSegment, Playlist};

use crate::hls::skip_filter::{SkipRange, SkipSegmentFilter};
use crate::proxy::handler::build_proxy_url;
use crate::utils::url::{resolve_url, segment_extension};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a proxied URL for a **manifest / sub-playlist** endpoint.
///
/// Format: `{proxy_base}/proxy/hls/manifest?d={encoded_url}&{passthrough_params}`
pub fn proxy_manifest_url(proxy_base: &str, destination: &str, params: &ProxyParams) -> String {
    build_proxy_url(
        proxy_base,
        Some("/proxy/hls/manifest"),
        destination,
        &HashMap::new(),
        &params.pass_headers,
        &HashMap::new(),
        &HashMap::new(),
        &[],
        None,
        None,
        params.api_password_opt(),
        None,
        None,
        false,
    )
    .expect("failed to build HLS manifest proxy URL")
}

/// Build a proxied URL for a **segment** endpoint.
///
/// Format: `{proxy_base}/proxy/hls/segment.{ext}?d={encoded_url}&{passthrough_params}`
pub fn proxy_segment_url(proxy_base: &str, destination: &str, params: &ProxyParams) -> String {
    let ext = segment_extension(destination);
    let mut query_params = HashMap::new();
    if let Some(playlist_url) = &params.playlist_url {
        query_params.insert("playlist_url".to_string(), playlist_url.clone());
    }

    build_proxy_url(
        proxy_base,
        Some(&format!("/proxy/hls/segment.{ext}")),
        destination,
        &query_params,
        &params.pass_headers,
        &HashMap::new(),
        &HashMap::new(),
        &[],
        None,
        None,
        params.api_password_opt(),
        None,
        None,
        false,
    )
    .expect("failed to build HLS segment proxy URL")
}

/// Build a proxied URL for a **key** endpoint.
/// Uses the segment endpoint path (no extension override).
pub fn proxy_key_url(proxy_base: &str, destination: &str, params: &ProxyParams) -> String {
    build_proxy_url(
        proxy_base,
        Some("/proxy/hls/segment"),
        destination,
        &HashMap::new(),
        &params.pass_headers,
        &HashMap::new(),
        &HashMap::new(),
        &[],
        None,
        None,
        params.api_password_opt(),
        None,
        None,
        false,
    )
    .expect("failed to build HLS key proxy URL")
}

// ---------------------------------------------------------------------------
// ProxyParams — context passed to the manifest processor
// ---------------------------------------------------------------------------

/// Parameters forwarded from the incoming request to all generated proxy URLs.
#[derive(Debug, Clone, Default)]
pub struct ProxyParams {
    /// Value of `api_password` query parameter, if any.
    pub api_password: String,
    /// `h_*` request headers to re-attach to proxied URLs.
    pub pass_headers: HashMap<String, String>,
    /// Upstream media playlist URL that produced segment proxy URLs.
    pub playlist_url: Option<String>,
}

impl ProxyParams {
    pub fn new(api_password: &str, pass_headers: HashMap<String, String>) -> Self {
        Self {
            api_password: api_password.to_string(),
            pass_headers,
            playlist_url: None,
        }
    }

    pub fn api_password_opt(&self) -> Option<&str> {
        if self.api_password.is_empty() {
            None
        } else {
            Some(self.api_password.as_str())
        }
    }

    pub fn with_playlist_url(mut self, playlist_url: &str) -> Self {
        self.playlist_url = Some(playlist_url.to_string());
        self
    }
}

// ---------------------------------------------------------------------------
// ManifestProcessor
// ---------------------------------------------------------------------------

/// Options that control manifest rewriting behaviour.
#[derive(Debug, Default)]
pub struct ManifestOptions {
    /// If set, only the key URL is proxied; segment URLs are returned direct.
    pub key_only_proxy: bool,
    /// If set, return all absolute URLs without any proxying.
    pub no_proxy: bool,
    /// Force all playlist/variant URLs through the proxy.
    pub force_playlist_proxy: bool,
    /// Skip ranges to filter out.
    pub skip_ranges: Vec<SkipRange>,
    /// Optional `EXT-X-START:TIME-OFFSET` value to inject.
    pub start_offset: Option<f64>,
    /// Inject `start_offset` even for VOD streams.
    pub force_start_offset: bool,
}

/// Processes an upstream M3U8 playlist, rewriting URLs through the local proxy.
pub struct ManifestProcessor {
    proxy_base: String,
    params: ProxyParams,
    opts: ManifestOptions,
}

impl ManifestProcessor {
    pub fn new(proxy_base: &str, params: ProxyParams, opts: ManifestOptions) -> Self {
        Self {
            proxy_base: proxy_base.to_string(),
            params,
            opts,
        }
    }

    /// Process the raw `content` of an M3U8 playlist fetched from `source_url`.
    ///
    /// Returns the modified playlist as a `String`.
    pub fn process(&self, content: &[u8], source_url: &str) -> String {
        // IPTV-style playlists use non-standard EXTINF attributes (group-title=,
        // tvg-id=, tvg-logo=, etc.) that m3u8-rs strips when serializing back.
        // Route them directly to the line-by-line processor to preserve all metadata.
        if is_iptv_playlist(content) {
            return self
                .process_lines(std::str::from_utf8(content).unwrap_or_default(), source_url);
        }
        match m3u8_rs::parse_playlist_res(content) {
            Ok(Playlist::MasterPlaylist(pl)) => self.process_master(pl, source_url),
            Ok(Playlist::MediaPlaylist(pl)) => self.process_media(pl, source_url),
            Err(_) => {
                // Try line-by-line fallback for non-standard playlists
                tracing::warn!(
                    "m3u8-rs failed to parse playlist from {}, using line fallback",
                    source_url
                );
                self.process_lines(std::str::from_utf8(content).unwrap_or_default(), source_url)
            }
        }
    }

    /// Extract absolute media segment URLs from an upstream media playlist.
    pub fn media_segment_urls(content: &[u8], source_url: &str) -> Vec<String> {
        match m3u8_rs::parse_playlist_res(content) {
            Ok(Playlist::MediaPlaylist(pl)) => pl
                .segments
                .iter()
                .map(|seg| resolve_url(source_url, &seg.uri))
                .filter(|url| !is_playlist_url(url))
                .collect(),
            Ok(Playlist::MasterPlaylist(_)) => Vec::new(),
            Err(_) => Self::media_segment_urls_from_lines(
                std::str::from_utf8(content).unwrap_or_default(),
                source_url,
            ),
        }
    }

    fn media_segment_urls_from_lines(content: &str, source_url: &str) -> Vec<String> {
        let mut urls = Vec::new();
        let mut pending_extinf = false;

        for line in content.lines().map(str::trim) {
            if line.starts_with("#EXTINF:") {
                pending_extinf = true;
                continue;
            }
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if pending_extinf {
                let resolved = resolve_url(source_url, line);
                if !is_playlist_url(&resolved) {
                    urls.push(resolved);
                }
                pending_extinf = false;
            }
        }

        urls
    }

    // -----------------------------------------------------------------------
    // Master playlist rewriting
    // -----------------------------------------------------------------------

    fn process_master(&self, mut pl: MasterPlaylist, base_url: &str) -> String {
        if self.opts.no_proxy {
            // Just resolve relative URLs to absolute
            for v in &mut pl.variants {
                v.uri = resolve_url(base_url, &v.uri);
            }
            for alt in &mut pl.alternatives {
                if let Some(ref uri) = alt.uri.clone() {
                    alt.uri = Some(resolve_url(base_url, uri));
                }
            }
        } else {
            for v in &mut pl.variants {
                v.uri = self.rewrite_playlist_uri(&v.uri, base_url);
            }
            for alt in &mut pl.alternatives {
                if let Some(ref uri) = alt.uri.clone() {
                    alt.uri = Some(self.rewrite_playlist_uri(uri, base_url));
                }
            }
            // m3u8-rs rejects certain valid-in-practice EXT-X-MEDIA attributes
            // (e.g. FORCED=NO on TYPE=AUDIO) and stores the whole tag in
            // unknown_tags, where write_to emits it verbatim.  Rewrite any URI=
            // found there so those rendition sub-playlists go through the proxy.
            for unknown in &mut pl.unknown_tags {
                if unknown.tag == "X-MEDIA" {
                    if let Some(ref rest) = unknown.rest.clone() {
                        if rest.contains("URI=\"") {
                            unknown.rest = Some(self.rewrite_tag_rest_uri(rest, base_url, true));
                        }
                    }
                }
            }
        }

        let mut out = Vec::new();
        pl.write_to(&mut out).unwrap_or_default();
        String::from_utf8_lossy(&out).into_owned()
    }

    // -----------------------------------------------------------------------
    // Media playlist rewriting
    // -----------------------------------------------------------------------

    fn process_media(&self, mut pl: MediaPlaylist, base_url: &str) -> String {
        let is_vod = pl.end_list
            || pl
                .playlist_type
                .as_ref()
                .map(|t| format!("{:?}", t).to_lowercase().contains("vod"))
                .unwrap_or(false);

        // Inject EXT-X-START if requested for live streams
        if let Some(offset) = self.opts.start_offset {
            if self.opts.force_start_offset || !is_vod {
                // EXT-X-START is a field on MediaPlaylist in m3u8-rs >= 6
                // We inject it via the unknown_tags mechanism if needed.
                // For now, inject as a raw tag — handled in line fallback.
                let _ = offset; // used below via write_to
            }
        }

        // Rewrite keys and map (init segment) URIs
        for seg in &mut pl.segments {
            // Key URI
            if let Some(ref mut key) = seg.key {
                if let Some(ref uri) = key.uri.clone() {
                    let resolved = resolve_url(base_url, uri);
                    key.uri = Some(if self.opts.no_proxy {
                        resolved
                    } else {
                        proxy_key_url(&self.proxy_base, &resolved, &self.params)
                    });
                }
            }

            // Init segment (EXT-X-MAP)
            if let Some(ref mut map) = seg.map {
                let resolved = resolve_url(base_url, &map.uri);
                map.uri = if self.opts.no_proxy {
                    resolved
                } else {
                    proxy_segment_url(&self.proxy_base, &resolved, &self.params)
                };
            }

            // Segment URI
            seg.uri = self.rewrite_segment_or_playlist_uri(&seg.uri, base_url);

            // Rewrite URI= values in unknown segment tags.
            //
            // m3u8-rs stores unrecognised tags (e.g. #EXT-X-MEDIA embedded in
            // VixCloud video sub-playlists) as ExtTag in MediaSegment.unknown_tags
            // and writes them back verbatim — leaving audio sub-playlist URIs
            // unproxied.  We fix them up here.
            for unknown in &mut seg.unknown_tags {
                if let Some(ref rest) = unknown.rest.clone() {
                    if rest.contains("URI=\"") {
                        // #EXT-X-MEDIA / #EXT-X-I-FRAME-STREAM-INF carry sub-playlist URIs;
                        // everything else (e.g. custom DRM tags) is treated as a key.
                        let is_playlist =
                            unknown.tag == "X-MEDIA" || unknown.tag == "X-I-FRAME-STREAM-INF";
                        unknown.rest = Some(self.rewrite_tag_rest_uri(rest, base_url, is_playlist));
                    }
                }
            }
        }

        // Rewrite URI= values in playlist-level unknown tags.
        //
        // If #EXT-X-MEDIA (or similar) appears before the first #EXTINF,
        // m3u8-rs places it in MediaPlaylist.unknown_tags rather than in any
        // MediaSegment.unknown_tags.  We handle both locations so that audio
        // and subtitle rendition sub-playlists are always proxied through the
        // manifest endpoint regardless of where the tag sits in the file.
        for unknown in &mut pl.unknown_tags {
            if let Some(ref rest) = unknown.rest.clone() {
                if rest.contains("URI=\"") {
                    let is_playlist =
                        unknown.tag == "X-MEDIA" || unknown.tag == "X-I-FRAME-STREAM-INF";
                    unknown.rest = Some(self.rewrite_tag_rest_uri(rest, base_url, is_playlist));
                }
            }
        }

        // Apply skip-segment filtering if configured
        let pl = if !self.opts.skip_ranges.is_empty() {
            self.apply_skip_filter(pl)
        } else {
            pl
        };

        let mut out = Vec::new();
        pl.write_to(&mut out).unwrap_or_default();
        let result = String::from_utf8_lossy(&out).into_owned();

        // Inject EXT-X-START after #EXTM3U if needed (m3u8-rs doesn't expose this field directly)
        if let Some(offset) = self.opts.start_offset {
            if self.opts.force_start_offset || !is_vod {
                return inject_ext_x_start(&result, offset);
            }
        }

        result
    }

    // -----------------------------------------------------------------------
    // Skip segment filtering
    // -----------------------------------------------------------------------

    fn apply_skip_filter(&self, mut pl: MediaPlaylist) -> MediaPlaylist {
        let ranges = self.opts.skip_ranges.clone();
        let mut filter = SkipSegmentFilter::new(ranges);
        let mut kept: Vec<MediaSegment> = Vec::with_capacity(pl.segments.len());
        let mut need_discontinuity = false;

        for seg in pl.segments.drain(..) {
            let duration = seg.duration as f64;
            if filter.check_and_advance(duration) {
                need_discontinuity = true;
            } else {
                let mut s = seg;
                if need_discontinuity {
                    s.discontinuity = true;
                    need_discontinuity = false;
                }
                kept.push(s);
            }
        }

        pl.segments = kept;
        pl
    }

    // -----------------------------------------------------------------------
    // URL rewriting helpers
    // -----------------------------------------------------------------------

    fn rewrite_playlist_uri(&self, uri: &str, base_url: &str) -> String {
        let abs = resolve_url(base_url, uri);
        proxy_manifest_url(&self.proxy_base, &abs, &self.params)
    }

    /// Rewrite the `URI="..."` value inside the **attribute string** of a parsed `ExtTag.rest`.
    ///
    /// Used to fix up `#EXT-X-MEDIA` (and similar) tags that land in
    /// `MediaSegment.unknown_tags` when VixCloud-style playlists embed audio
    /// rendition references inside video sub-playlists.
    ///
    /// `is_playlist` — if `true` the URI points to a sub-playlist and is
    /// routed through the manifest endpoint; otherwise the key endpoint is used.
    fn rewrite_tag_rest_uri(&self, rest: &str, base_url: &str, is_playlist: bool) -> String {
        let Some(start) = rest.find("URI=\"") else {
            return rest.to_string();
        };
        let after_quote = start + 5; // skip 'URI="'
        let Some(end) = rest[after_quote..].find('"') else {
            return rest.to_string();
        };
        let original_uri = &rest[after_quote..after_quote + end];
        let resolved = resolve_url(base_url, original_uri);

        let new_uri = if self.opts.no_proxy {
            resolved
        } else if is_playlist {
            proxy_manifest_url(&self.proxy_base, &resolved, &self.params)
        } else {
            proxy_key_url(&self.proxy_base, &resolved, &self.params)
        };

        rest.replacen(
            &format!("URI=\"{}\"", original_uri),
            &format!("URI=\"{}\"", new_uri),
            1,
        )
    }

    fn rewrite_segment_or_playlist_uri(&self, uri: &str, base_url: &str) -> String {
        if self.opts.no_proxy {
            return resolve_url(base_url, uri);
        }
        if self.opts.key_only_proxy {
            // Only key is proxied; return segment directly
            return resolve_url(base_url, uri);
        }

        let abs = resolve_url(base_url, uri);

        // Sub-playlists (e.g. variant streams embedded in a media playlist)
        if is_playlist_url(&abs) {
            return proxy_manifest_url(&self.proxy_base, &abs, &self.params);
        }

        // If forced-playlist-proxy is on, route everything as a manifest
        if self.opts.force_playlist_proxy {
            return proxy_manifest_url(&self.proxy_base, &abs, &self.params);
        }

        proxy_segment_url(&self.proxy_base, &abs, &self.params)
    }

    // -----------------------------------------------------------------------
    // Line-by-line fallback for non-standard playlists
    // -----------------------------------------------------------------------

    fn process_lines(&self, content: &str, base_url: &str) -> String {
        let mut out = String::with_capacity(content.len() + 256);
        let mut skip_filter = if !self.opts.skip_ranges.is_empty() {
            Some(SkipSegmentFilter::new(self.opts.skip_ranges.clone()))
        } else {
            None
        };
        let mut pending_extinf: Option<String> = None;
        let mut need_discontinuity = false;
        let mut start_offset_injected = false;

        for line in content.lines() {
            // EXT-X-START injection point
            if line.trim() == "#EXTM3U" {
                out.push_str(line);
                out.push('\n');
                if let Some(offset) = self.opts.start_offset {
                    if !start_offset_injected {
                        out.push_str(&format!(
                            "#EXT-X-START:TIME-OFFSET={:.1},PRECISE=YES\n",
                            offset
                        ));
                        start_offset_injected = true;
                    }
                }
                continue;
            }

            // EXTINF line
            if line.starts_with("#EXTINF:") {
                let duration = parse_extinf_duration(line);
                if let Some(ref mut f) = skip_filter {
                    if f.check_and_advance(duration) {
                        // Will skip this segment
                        need_discontinuity = true;
                        pending_extinf = None;
                        continue;
                    }
                }
                pending_extinf = Some(line.to_string());
                continue;
            }

            // Segment URL line
            if !line.starts_with('#') && !line.trim().is_empty() {
                if skip_filter.is_some() && pending_extinf.is_none() {
                    // Segment was skipped
                    continue;
                }
                if need_discontinuity {
                    out.push_str("#EXT-X-DISCONTINUITY\n");
                    need_discontinuity = false;
                }
                if let Some(extinf) = pending_extinf.take() {
                    out.push_str(&extinf);
                    out.push('\n');
                }
                out.push_str(&self.rewrite_segment_or_playlist_uri(line, base_url));
                out.push('\n');
                continue;
            }

            // Key / map URI line
            if line.contains("URI=") {
                out.push_str(&self.process_tag_with_uri(line, base_url));
                out.push('\n');
                continue;
            }

            // EXT-X-DISCONTINUITY — pass through, reset pending
            if line.starts_with("#EXT-X-DISCONTINUITY") {
                out.push_str(line);
                out.push('\n');
                need_discontinuity = false;
                continue;
            }

            // All other lines
            out.push_str(line);
            out.push('\n');
        }

        out
    }

    /// Rewrite the `URI="..."` value within a tag line (`#EXT-X-KEY`, `#EXT-X-MAP`, etc.).
    fn process_tag_with_uri(&self, line: &str, base_url: &str) -> String {
        // Extract URI value
        let Some(start) = line.find("URI=\"") else {
            return line.to_string();
        };
        let after_quote = start + 5; // skip 'URI="'
        let Some(end) = line[after_quote..].find('"') else {
            return line.to_string();
        };
        let original_uri = &line[after_quote..after_quote + end];
        let resolved = resolve_url(base_url, original_uri);

        let new_uri = if self.opts.no_proxy {
            resolved
        } else if line.starts_with("#EXT-X-MAP") {
            proxy_segment_url(&self.proxy_base, &resolved, &self.params)
        } else if line.starts_with("#EXT-X-MEDIA") {
            // EXT-X-MEDIA URI is a rendition sub-playlist — route through manifest endpoint
            proxy_manifest_url(&self.proxy_base, &resolved, &self.params)
        } else {
            // Key or other URI tag → use key endpoint
            proxy_key_url(&self.proxy_base, &resolved, &self.params)
        };

        line.replacen(
            &format!("URI=\"{}\"", original_uri),
            &format!("URI=\"{}\"", new_uri),
            1,
        )
    }
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

/// Parse the duration value from an `#EXTINF:<duration>[,<title>]` line.
fn parse_extinf_duration(line: &str) -> f64 {
    line.strip_prefix("#EXTINF:")
        .and_then(|rest| rest.split(',').next())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn is_playlist_url(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".m3u8") || lower.ends_with(".m3u")
}

/// Returns true if the content is an IPTV-style extended m3u8 playlist that
/// carries non-standard EXTINF attributes (group-title=, tvg-id=, tvg-name=,
/// tvg-logo=).  m3u8-rs parses these successfully but strips the custom
/// attributes on write_to, so we must use the line-by-line fallback to
/// preserve all metadata.
fn is_iptv_playlist(content: &[u8]) -> bool {
    let text = std::str::from_utf8(content).unwrap_or_default();
    text.lines().any(|line| {
        let t = line.trim();
        t.starts_with("#EXTINF:")
            && (t.contains("group-title=")
                || t.contains("tvg-id=")
                || t.contains("tvg-name=")
                || t.contains("tvg-logo="))
    })
}

/// Inject `#EXT-X-START:TIME-OFFSET=<offset>,PRECISE=YES` right after `#EXTM3U`.
fn inject_ext_x_start(content: &str, offset: f64) -> String {
    if let Some(pos) = content.find("#EXTM3U") {
        let after = pos + "#EXTM3U".len();
        // Find end of the #EXTM3U line
        let nl = content[after..]
            .find('\n')
            .map(|i| after + i + 1)
            .unwrap_or(after);
        let tag = format!("#EXT-X-START:TIME-OFFSET={:.1},PRECISE=YES\n", offset);
        format!("{}{}{}", &content[..nl], tag, &content[nl..])
    } else {
        content.to_string()
    }
}

/// Generate a minimal valid M3U8 that signals a normal stream end.
pub fn graceful_end_playlist(message: &str) -> String {
    format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-PLAYLIST-TYPE:VOD\n# {}\n#EXT-X-ENDLIST\n",
        message
    )
}

/// Generate a minimal valid M3U8 for error scenarios.
pub fn error_playlist(error_message: &str) -> String {
    format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-PLAYLIST-TYPE:VOD\n# Error: {}\n#EXT-X-ENDLIST\n",
        error_message
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::encryption::EncryptionHandler;

    fn extract_token_from_url(url: &str) -> &str {
        let (_, rest) = url
            .split_once("/_token_")
            .unwrap_or_else(|| panic!("expected tokenized URL, got: {url}"));
        let end = rest.find('/').unwrap_or(rest.len());
        &rest[..end]
    }

    fn extract_uri_value(line: &str) -> Option<&str> {
        let start = line.find("URI=\"")? + 5;
        let end = line[start..].find('"')?;
        Some(&line[start..start + end])
    }

    fn default_processor(proxy_base: &str) -> ManifestProcessor {
        ManifestProcessor::new(
            proxy_base,
            ProxyParams::new("secret", HashMap::new()),
            ManifestOptions::default(),
        )
    }

    #[test]
    fn test_proxy_segment_url_ts() {
        let params = ProxyParams::new("pass", HashMap::new());
        let url = proxy_segment_url(
            "http://proxy:8888",
            "https://cdn.example.com/seg001.ts",
            &params,
        );
        assert!(url.starts_with("http://proxy:8888/_token_"));
        assert!(url.ends_with("/proxy/hls/segment.ts"));

        let token = extract_token_from_url(&url);
        let pd = EncryptionHandler::new(b"pass")
            .unwrap()
            .decrypt(token, None)
            .unwrap();
        assert_eq!(pd.destination, "https://cdn.example.com/seg001.ts");
    }

    #[test]
    fn test_proxy_segment_url_includes_playlist_url() {
        let params = ProxyParams::new("pass", HashMap::new())
            .with_playlist_url("https://cdn.example.com/live.m3u8");
        let url = proxy_segment_url(
            "http://proxy:8888",
            "https://cdn.example.com/seg001.ts",
            &params,
        );
        let token = extract_token_from_url(&url);
        let pd = EncryptionHandler::new(b"pass")
            .unwrap()
            .decrypt(token, None)
            .unwrap();

        assert_eq!(pd.destination, "https://cdn.example.com/seg001.ts");
        assert_eq!(
            pd.query_params
                .as_ref()
                .and_then(|v| v.get("playlist_url"))
                .and_then(|v| v.as_str()),
            Some("https://cdn.example.com/live.m3u8")
        );
    }

    #[test]
    fn test_proxy_manifest_url() {
        let params = ProxyParams::new("pass", HashMap::new());
        let url = proxy_manifest_url(
            "http://proxy:8888",
            "https://cdn.example.com/playlist.m3u8",
            &params,
        );
        assert!(url.starts_with("http://proxy:8888/_token_"));
        assert!(url.ends_with("/proxy/hls/manifest"));
    }

    #[test]
    fn test_proxy_urls_preserve_public_path_prefix() {
        let params = ProxyParams::new("pass", HashMap::new());
        let base = "https://proxy.example.test/mediaflow/prefix";

        let manifest = proxy_manifest_url(base, "https://cdn.example.com/master.m3u8", &params);
        let segment = proxy_segment_url(base, "https://cdn.example.com/seg001.ts", &params);
        let key = proxy_key_url(base, "https://cdn.example.com/key.bin", &params);

        assert!(manifest.starts_with("https://proxy.example.test/mediaflow/prefix/_token_"));
        assert!(manifest.ends_with("/proxy/hls/manifest"));
        assert!(segment.starts_with("https://proxy.example.test/mediaflow/prefix/_token_"));
        assert!(segment.ends_with("/proxy/hls/segment.ts"));
        assert!(key.starts_with("https://proxy.example.test/mediaflow/prefix/_token_"));
        assert!(key.ends_with("/proxy/hls/segment"));
    }

    #[test]
    fn test_process_media_playlist() {
        let processor = default_processor("http://proxy:8888");
        let m3u8 = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n\
            #EXTINF:10.0,\nseg001.ts\n#EXTINF:10.0,\nseg002.ts\n#EXT-X-ENDLIST\n";

        let result = processor.process(m3u8, "https://cdn.example.com/playlist.m3u8");
        let segment_url = result
            .lines()
            .find(|line| !line.is_empty() && !line.starts_with('#'))
            .expect("expected segment URL");

        assert!(segment_url.starts_with("http://proxy:8888/_token_"));
        assert!(segment_url.ends_with("/proxy/hls/segment.ts"));

        let token = extract_token_from_url(segment_url);
        let pd = EncryptionHandler::new(b"secret")
            .unwrap()
            .decrypt(token, None)
            .unwrap();
        assert_eq!(pd.destination, "https://cdn.example.com/seg001.ts");
        // Should NOT contain original relative URLs
        assert!(!result.contains("\nseg001.ts\n"));
    }

    #[test]
    fn test_process_master_playlist() {
        let processor = default_processor("http://proxy:8888");
        let m3u8 = b"#EXTM3U\n#EXT-X-VERSION:3\n\
            #EXT-X-STREAM-INF:BANDWIDTH=1400000\nhigh/playlist.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=400000\nlow/playlist.m3u8\n";

        let result = processor.process(m3u8, "https://cdn.example.com/master.m3u8");
        let variant_url = result
            .lines()
            .find(|line| !line.is_empty() && !line.starts_with('#'))
            .expect("expected variant URL");

        assert!(variant_url.starts_with("http://proxy:8888/_token_"));
        assert!(variant_url.ends_with("/proxy/hls/manifest"));
    }

    #[test]
    fn test_process_master_playlist_resolves_relative_variants_against_effective_url() {
        let processor = default_processor("http://proxy:8888");
        let m3u8 = b"#EXTM3U\n#EXT-X-VERSION:3\n\
            #EXT-X-STREAM-INF:BANDWIDTH=1400000\nhigh/playlist.m3u8\n";

        let result = processor.process(m3u8, "https://edge.example.com/live/master.m3u8");
        let variant_url = result
            .lines()
            .find(|line| !line.is_empty() && !line.starts_with('#'))
            .expect("expected variant URL");
        let token = extract_token_from_url(variant_url);
        let pd = EncryptionHandler::new(b"secret")
            .unwrap()
            .decrypt(token, None)
            .unwrap();

        assert_eq!(
            pd.destination,
            "https://edge.example.com/live/high/playlist.m3u8"
        );
    }

    #[test]
    fn test_media_segment_urls_extracts_only_media_segments() {
        let media = b"#EXTM3U\n#EXT-X-TARGETDURATION:10\n\
            #EXTINF:10.0,\nseg001.ts\n#EXTINF:10.0,\nhttps://cdn.example.com/seg002.ts\n\
            #EXT-X-ENDLIST\n";
        let urls =
            ManifestProcessor::media_segment_urls(media, "https://cdn.example.com/path/live.m3u8");

        assert_eq!(
            urls,
            vec![
                "https://cdn.example.com/path/seg001.ts".to_string(),
                "https://cdn.example.com/seg002.ts".to_string(),
            ]
        );

        let master = b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nvariant.m3u8\n";
        assert!(ManifestProcessor::media_segment_urls(
            master,
            "https://cdn.example.com/master.m3u8"
        )
        .is_empty());
    }

    #[test]
    fn test_media_segment_urls_use_effective_manifest_url_as_base() {
        let media = b"#EXTM3U\n#EXT-X-TARGETDURATION:10\n\
            #EXTINF:10.0,\nsegments/seg001.ts\n#EXT-X-ENDLIST\n";
        let urls = ManifestProcessor::media_segment_urls(
            media,
            "https://edge.example.com/live/redirected/manifest.m3u8",
        );

        assert_eq!(
            urls,
            vec!["https://edge.example.com/live/redirected/segments/seg001.ts".to_string()]
        );
    }

    #[test]
    fn test_no_proxy_mode() {
        let processor = ManifestProcessor::new(
            "http://proxy:8888",
            ProxyParams::new("pass", HashMap::new()),
            ManifestOptions {
                no_proxy: true,
                ..Default::default()
            },
        );
        let m3u8 = b"#EXTM3U\n#EXT-X-TARGETDURATION:10\n\
            #EXTINF:10.0,\nseg001.ts\n#EXT-X-ENDLIST\n";

        let result = processor.process(m3u8, "https://cdn.example.com/playlist.m3u8");

        // Should contain the absolute URL but NOT the proxy path
        assert!(result.contains("cdn.example.com"));
        assert!(!result.contains("/proxy/hls/segment"));
    }

    #[test]
    fn test_inject_ext_x_start() {
        let content = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n";
        let result = inject_ext_x_start(content, -30.0);
        assert!(result.contains("#EXT-X-START:TIME-OFFSET=-30.0,PRECISE=YES"));
        // Must appear after #EXTM3U line
        let extm3u_pos = result.find("#EXTM3U").unwrap();
        let start_pos = result.find("#EXT-X-START").unwrap();
        assert!(start_pos > extm3u_pos);
    }

    /// When a video sub-playlist embeds #EXT-X-MEDIA audio references inline,
    /// m3u8-rs stores them in MediaSegment.unknown_tags and writes them back
    /// verbatim — we must rewrite the URI through the manifest proxy.
    #[test]
    fn test_process_media_with_embedded_ext_x_media() {
        let processor = default_processor("http://proxy:8888");
        let m3u8 = concat!(
            "#EXTM3U\n",
            "#EXT-X-VERSION:3\n",
            "#EXT-X-TARGETDURATION:6\n",
            "#EXT-X-MEDIA-SEQUENCE:0\n",
            // EXT-X-MEDIA embedded inside a video sub-playlist
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio-0\",NAME=\"lang\",DEFAULT=YES,",
            "URI=\"https://upstream.example.com/playlist?type=audio&rendition=lang&token=TOK\"\n",
            "#EXT-X-KEY:METHOD=AES-128,URI=\"https://upstream.example.com/enc.key\",IV=0x0000\n",
            "#EXTINF:6.0,\n",
            "https://cdn.example.com/seg001.ts\n",
            "#EXT-X-ENDLIST\n",
        );
        let result = processor.process(
            m3u8.as_bytes(),
            "https://upstream.example.com/playlist?type=video&rendition=hd",
        );
        let tokenized_uri = result
            .lines()
            .find(|line| line.starts_with("#EXT-X-MEDIA"))
            .and_then(extract_uri_value)
            .expect("expected tokenized media URI");

        assert!(tokenized_uri.contains("/_token_"));
        assert!(tokenized_uri.ends_with("/proxy/hls/manifest"));
        // Must NOT contain the bare audio URL
        assert!(
            !result.contains("URI=\"https://upstream.example.com/playlist?type=audio"),
            "Audio URI is still bare. Got:\n{}",
            result
        );
    }

    /// m3u8-rs rejects FORCED=NO on TYPE=AUDIO (only valid on SUBTITLES) and
    /// puts the whole tag into MasterPlaylist.unknown_tags, where write_to emits
    /// it verbatim with an unproxied URI.  We must catch and rewrite it.
    #[test]
    fn test_process_master_audio_forced_no_proxied() {
        let processor = default_processor("http://proxy:8888");
        let m3u8 = concat!(
            "#EXTM3U\n",
            "#EXT-X-VERSION:3\n",
            // FORCED=NO on AUDIO — m3u8-rs rejects this → goes into unknown_tags
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"lang1\",",
            "DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,LANGUAGE=\"ita\",",
            "URI=\"https://upstream.example.com/playlist?type=audio&rendition=ita&token=TOK\"\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=3000000,AUDIO=\"audio\"\n",
            "https://upstream.example.com/playlist?type=video&rendition=hd&token=VID\n",
        );
        let result = processor.process(m3u8.as_bytes(), "https://upstream.example.com/master.m3u8");
        let tokenized_uri = result
            .lines()
            .find_map(extract_uri_value)
            .expect("expected tokenized URI");

        assert!(tokenized_uri.contains("/_token_"));
        assert!(tokenized_uri.ends_with("/proxy/hls/manifest"));
        assert!(
            !result.contains("URI=\"https://upstream.example.com/playlist?type=audio"),
            "Audio URI is still bare. Got:\n{}",
            result
        );
    }

    /// IPTV-style m3u8 playlists use non-standard EXTINF attributes that m3u8-rs
    /// strips on write_to.  The proxy must preserve group-title, tvg-id, etc.
    /// so that IPTV players can group channels correctly (issue #21).
    #[test]
    fn test_iptv_playlist_preserves_extinf_attributes() {
        let processor = ManifestProcessor::new(
            "http://proxy:8888",
            ProxyParams::new("secret", HashMap::new()),
            ManifestOptions {
                force_playlist_proxy: true,
                ..Default::default()
            },
        );
        let m3u8 = concat!(
            "#EXTM3U x-tvg-url=\"http://epg.example.com/epg.xml\"\n",
            "#EXTINF:-1 tvg-id=\"ch1\" tvg-name=\"Channel 1\" tvg-logo=\"http://logo.example.com/ch1.png\" group-title=\"Sports\",Channel 1\n",
            "http://stream.example.com/ch1\n",
            "#EXTINF:-1 tvg-id=\"ch2\" tvg-name=\"Channel 2\" tvg-logo=\"http://logo.example.com/ch2.png\" group-title=\"News\",Channel 2\n",
            "http://stream.example.com/ch2\n",
        );

        let result = processor.process(m3u8.as_bytes(), "http://upstream.example.com/playlist.m3u8");

        assert!(result.contains("group-title=\"Sports\""), "group-title stripped:\n{result}");
        assert!(result.contains("group-title=\"News\""), "group-title stripped:\n{result}");
        assert!(result.contains("tvg-id=\"ch1\""), "tvg-id stripped:\n{result}");
        assert!(result.contains("tvg-name=\"Channel 1\""), "tvg-name stripped:\n{result}");
        assert!(result.contains("tvg-logo="), "tvg-logo stripped:\n{result}");
        // Channel URLs must be proxied through manifest endpoint
        assert!(result.contains("/proxy/hls/manifest"), "URLs not proxied:\n{result}");
        // Must not contain bare stream URLs
        assert!(
            !result.contains("http://stream.example.com/ch1\n"),
            "bare stream URL present:\n{result}"
        );
    }

    #[test]
    fn test_process_master_ext_x_media_proxied() {
        let processor = default_processor("http://proxy:8888");
        // Master playlist with EXT-X-MEDIA whose URI contains & in the query string
        let m3u8 = concat!(
            "#EXTM3U\n",
            "#EXT-X-VERSION:3\n",
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio-0\",NAME=\"lang1\",DEFAULT=YES,AUTOSELECT=YES,",
            "URI=\"https://upstream.example.com/playlist?type=audio&rendition=lang1&token=TOK\"\n",
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio-0\",NAME=\"lang2\",DEFAULT=NO,AUTOSELECT=YES,",
            "URI=\"https://upstream.example.com/playlist?type=audio&rendition=lang2&token=TOK\"\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=3000000,AUDIO=\"audio-0\"\n",
            "https://upstream.example.com/playlist?type=video&rendition=hd&token=VID\n",
        );
        let result = processor.process(m3u8.as_bytes(), "https://upstream.example.com/master.m3u8");
        let tokenized_uri = result
            .lines()
            .find_map(extract_uri_value)
            .expect("expected tokenized URI");

        assert!(tokenized_uri.contains("/_token_"));
        assert!(tokenized_uri.ends_with("/proxy/hls/manifest"));
        // The audio URI must NOT appear bare
        assert!(
            !result.contains("URI=\"https://upstream.example.com/playlist?type=audio"),
            "Audio sub-playlist URI is still bare (unproxied). Got:\n{}",
            result
        );
    }

    #[test]
    fn test_process_media_playlist_resolves_relative_key_map_and_segment_against_effective_url() {
        let processor = default_processor("http://proxy:8888");
        let m3u8 = concat!(
            "#EXTM3U\n",
            "#EXT-X-VERSION:7\n",
            "#EXT-X-TARGETDURATION:6\n",
            "#EXT-X-KEY:METHOD=AES-128,URI=\"keys/key.bin\"\n",
            "#EXT-X-MAP:URI=\"init/init.mp4\"\n",
            "#EXTINF:6.0,\n",
            "segments/seg001.ts\n",
            "#EXT-X-ENDLIST\n",
        );

        let result = processor.process(
            m3u8.as_bytes(),
            "https://edge.example.com/live/redirected/playlist.m3u8",
        );
        let mut token_destinations = Vec::new();
        let decrypt = EncryptionHandler::new(b"secret").unwrap();
        for line in result.lines() {
            if let Some(url) = extract_uri_value(line) {
                let token = extract_token_from_url(url);
                let pd = decrypt.decrypt(token, None).unwrap();
                token_destinations.push(pd.destination);
            } else if !line.is_empty() && !line.starts_with('#') {
                let token = extract_token_from_url(line);
                let pd = decrypt.decrypt(token, None).unwrap();
                token_destinations.push(pd.destination);
            }
        }

        assert!(token_destinations
            .contains(&"https://edge.example.com/live/redirected/keys/key.bin".to_string()));
        assert!(token_destinations
            .contains(&"https://edge.example.com/live/redirected/init/init.mp4".to_string()));
        assert!(token_destinations
            .contains(&"https://edge.example.com/live/redirected/segments/seg001.ts".to_string()));
    }
}
