//! YouTube extraction + the local HTTP proxy — a port of Go's `radio.Resolver`
//! (`internal/radio/radio.go`), backed by `rusty_ytdl` instead of
//! `kkdai/youtube`. This is the XL fidelity risk (signature-cipher + format
//! selection drift with YouTube changes); it sits behind the
//! [`StreamResolver`](crate::StreamResolver) / [`PlaylistExpander`] traits so the
//! rest of the spine stays green if extraction breaks.
//!
//! * Livestreams with a DASH manifest → the local `/dash` proxy (static-DASH
//!   path), reported NOT live so EOS re-resolves a fresh window.
//! * HLS-only livestreams → the HLS URL, reported live.
//! * VOD → an audio-only format (itag 140 AAC preferred), routed through the
//!   local `/stream` proxy + byte-cache.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Query, State, Path as AxumPath};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use once_cell::sync::Lazy;
use regex::{Captures, Regex};
use parking_lot::Mutex;
use serde::Deserialize;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

use rusty_ytdl::search::{Playlist, PlaylistSearchOptions};
use rusty_ytdl::{Video, VideoFormat, VideoOptions, RequestOptions};

use crate::error::{Error, Result};
use crate::event::ResolvedTrack;
use crate::format::{pick_audio_index, FormatLike};
use crate::proxy::{download_into, ByteCacheMap, VideoCache};
use crate::resolver::{PlaylistExpander, StreamResolver};
use crate::staticize::staticize_live_mpd;

/// YouTube signs stream URLs with ~6h expiry; refresh well before. Playlist
/// membership changes far less, so a much longer TTL. (Go constants.)
const CACHE_TTL: Duration = Duration::from_secs(3600);
const PLAYLIST_CACHE_TTL: Duration = Duration::from_secs(6 * 3600);
/// dashdemux refetches dynamic manifests every ~2s (~1MB each); cache briefly.
const DASH_BODY_TTL: Duration = Duration::from_millis(1500);

/// Caps concurrent upstream segment fetches. Bounds the deep readahead's startup
/// fan-out so it can't open dozens of googlevideo connections at once (→ 403
/// storm). The on-demand player request acquires this blocking (priority);
/// readahead prefetch yields to it (try-acquire), so prefetch can never starve
/// the segment the player needs now. Overridable via `CLAWD_SEG_CONCURRENCY`.
const UPSTREAM_CONCURRENCY: usize = 8;
/// Readahead depth: how many segments ahead of the player the proxy keeps warm in
/// RAM. ~2s/segment, so 40 ≈ 80s — comfortably more than one adaptivedemux buffer
/// cycle (~50s), the invariant that turns refill bursts into RAM cache hits.
const READAHEAD_DEPTH: i64 = 40;
/// How many segments past the observed served edge (`max_served_sq`) prefetch may
/// probe — enough to discover the edge advancing, without a 403 storm on
/// not-yet-published segments. Overridable via `CLAWD_PREFETCH_PROBE`.
const PREFETCH_PROBE_MARGIN: i64 = 4;
/// Segment-cache TTL. Must exceed the readahead horizon plus one buffer cycle so a
/// deep-prefetched segment survives until the player reaches it.
const SEGMENT_CACHE_TTL_S: u64 = 180;
static UPSTREAM_SEM: Lazy<tokio::sync::Semaphore> = Lazy::new(|| {
    let n = std::env::var("CLAWD_SEG_CONCURRENCY").ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(UPSTREAM_CONCURRENCY);
    tokio::sync::Semaphore::new(n.max(1))
});

static RE_BASE_URL: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?s)<BaseURL\b([^>]*)>(.*?)</BaseURL>"#).unwrap());
static RE_SQ: Lazy<Regex> = Lazy::new(|| Regex::new(r#"/sq/(\d+)/"#).unwrap());

#[derive(Clone, Debug)]
enum SegmentStatus {
    Pending,
    Done(Bytes),
    Failed,
}

#[derive(Clone)]
enum SegmentState {
    InFlight(tokio::sync::watch::Receiver<SegmentStatus>),
    Complete(Bytes),
}

struct SegmentEntry {
    state: SegmentState,
    at: Instant,
}


struct TrackEntry {
    track: ResolvedTrack,
    at: Instant,
}
struct PlaylistEntry {
    ids: Vec<String>,
    at: Instant,
}
struct ManifestBody {
    body: Bytes,
    at: Instant,
}

/// Resolver + local proxy. Construct with [`YtdlResolver::new`]; lives for the
/// process (the proxy server task holds a strong `Arc`, matching Go's
/// never-freed `radio.Resolver`).
pub struct YtdlResolver {
    rt: Handle,
    http: reqwest::Client,
    http_v4: reqwest::Client,
    http_v6: reqwest::Client,
    port: u16,
    cache: Mutex<HashMap<String, TrackEntry>>,
    playlist_cache: Mutex<HashMap<String, PlaylistEntry>>,
    byte_cache: Mutex<ByteCacheMap>,
    live_dash: Mutex<HashMap<String, String>>,
    live_dash_body: Mutex<HashMap<String, ManifestBody>>,
    bound_clients: Mutex<HashMap<std::net::IpAddr, reqwest::Client>>,
    segment_cache: Mutex<HashMap<String, SegmentEntry>>,
    /// Highest segment `sq` per video the CDN has actually served (HTTP 200/206),
    /// learned from real fetches. Prefetch probes only just past this so it doesn't
    /// storm googlevideo with 403s on not-yet-published future segments.
    max_served_sq: Mutex<HashMap<String, i64>>,
}

impl YtdlResolver {
    /// Binds the local proxy to `127.0.0.1:0` and starts serving on `rt`. Errors
    /// only if the listener can't bind; extraction failures surface later.
    pub fn new(rt: Handle) -> Result<Arc<Self>> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .map_err(|e| Error::new(format!("radio: proxy bind: {e}")))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| Error::new(format!("radio: proxy nonblocking: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| Error::new(format!("radio: proxy addr: {e}")))?
            .port();

        // googlevideo IP-locks the deciphered URL AND rejects unexpected
        // User-Agents with a 403 — fetch with a desktop-browser UA so the
        // byte-cache + passthrough are accepted (the proxy's reason to exist).
        let http = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
            .local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        let http_v4 = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
            .local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        let http_v6 = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
            .local_address(std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED))
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        let resolver = Arc::new(YtdlResolver {
            rt: rt.clone(),
            http,
            http_v4,
            http_v6,
            port,
            cache: Mutex::new(HashMap::new()),
            playlist_cache: Mutex::new(HashMap::new()),
            byte_cache: Mutex::new(ByteCacheMap::default()),
            live_dash: Mutex::new(HashMap::new()),
            live_dash_body: Mutex::new(HashMap::new()),
            bound_clients: Mutex::new(HashMap::new()),
            segment_cache: Mutex::new(HashMap::new()),
            max_served_sq: Mutex::new(HashMap::new()),
        });

        let app = Router::new()
            .route("/stream", get(stream_handler))
            .route("/dash", get(dash_handler))
            .route("/hls/master", get(hls_master_handler))
            .route("/hls/sub", get(hls_sub_handler))
            .route("/proxy_segment/*path", get(proxy_segment_handler))
            .with_state(resolver.clone());
        rt.spawn(async move {
            match tokio::net::TcpListener::from_std(listener) {
                Ok(l) => {
                    if let Err(e) = axum::serve(l, app).await {
                        log::warn!("[radio] proxy server exited: {e}");
                    }
                }
                Err(e) => log::warn!("[radio] proxy listener init failed: {e}"),
            }
        });
        Ok(resolver)
    }

    /// The local proxy port (0 if unbound).
    pub fn port(&self) -> u16 {
        self.port
    }

    fn proxy_prefix(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    /// Resolves the underlying stream URL (HLS manifest, `/dash` proxy URL, or a
    /// direct googlevideo VOD URL). (Go `resolveDirect`.)
    async fn resolve_direct(&self, video_id: &str, force_refresh: bool) -> Result<ResolvedTrack> {
        let video_id = video_id.trim();
        if video_id.is_empty() {
            return Err(Error::new("radio: empty video id"));
        }
        if !force_refresh {
            let cache = self.cache.lock();
            if let Some(e) = cache.get(video_id) {
                if !e.track.url.is_empty() && e.at.elapsed() < CACHE_TTL {
                    return Ok(e.track.clone());
                }
            }
        }

        let options = VideoOptions {
            request_options: RequestOptions {
                client: Some(self.http.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        let video = Video::new_with_options(video_id, options)
            .map_err(|e| Error::new(format!("youtube: new video {video_id}: {e}")))?;
        let info = video
            .get_info()
            .await
            .map_err(|e| Error::new(format!("youtube: get video info for {video_id}: {e}")))?;

        // Livestream: prefer the DASH manifest (separate fMP4 audio track; HLS is
        // muxed MPEG-TS that clicks every segment). The manifest goes through our
        // local proxy, which staticizes the dynamic live MPD; reported NOT live so
        // playback EOSes at the window end and the station re-resolves a fresh one.
        if self.port != 0 {
            if let Some(dash) = info.dash_manifest_url.as_deref() {
                if !dash.is_empty() {
                    self.live_dash.lock().insert(video_id.to_string(), dash.to_string());
                    self.live_dash_body.lock().remove(video_id);
                    let track = ResolvedTrack {
                        url: format!("http://127.0.0.1:{}/dash?id={}", self.port, video_id),
                        is_live: false,
                    };
                    self.store_track(video_id, &track);
                    return Ok(track);
                }
            }
        }
        if let Some(hls) = info.hls_manifest_url.as_deref() {
            if !hls.is_empty() {
                let track = ResolvedTrack { url: hls.to_string(), is_live: true };
                self.store_track(video_id, &track);
                return Ok(track);
            }
        }

        // VOD: pick an audio-only format (audio/mp4 itag 140 AAC preferred). Only
        // consider formats rusty_ytdl actually deciphered (non-empty url): the
        // adaptive audio itags are often signature-ciphered and left empty when
        // the bundled decipher drifts from YouTube's player JS, in which case the
        // muxed itag-18 (audio+video mp4) is the playable fallback — gstreamer's
        // AUDIO playbin flag decodes only its audio track.
        let usable: Vec<(usize, FmtAdapter)> = info
            .formats
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.url.is_empty())
            .map(|(i, f)| (i, FmtAdapter::from(f)))
            .collect();
        if usable.is_empty() {
            return Err(Error::new(format!("youtube: no deciphered format for {video_id}")));
        }
        let adapters: Vec<FmtAdapter> = usable.iter().map(|(_, a)| a.clone()).collect();
        let pick = pick_audio_index(&adapters)
            .ok_or_else(|| Error::new(format!("youtube: no playable format for {video_id}")))?;
        let url = info.formats[usable[pick].0].url.clone();
        let track = ResolvedTrack { url, is_live: false };
        self.store_track(video_id, &track);
        Ok(track)
    }

    fn store_track(&self, video_id: &str, track: &ResolvedTrack) {
        self.cache.lock().insert(
            video_id.to_string(),
            TrackEntry { track: track.clone(), at: Instant::now() },
        );
    }

    /// Returns the cache for `video_id`, kicking off its one-time background
    /// download on first request. (Go `getOrStartCache`.)
    fn get_or_start_cache(self: &Arc<Self>, video_id: &str) -> Arc<VideoCache> {
        let mut bc = self.byte_cache.lock();
        if let Some(vc) = bc.get(video_id) {
            return vc;
        }
        let token = CancellationToken::new();
        let vc = VideoCache::new(token.clone());
        bc.insert(video_id, vc.clone());
        drop(bc);

        let me = Arc::clone(self);
        let id = video_id.to_string();
        let vc2 = vc.clone();
        self.rt.spawn(async move {
            match me.resolve_direct(&id, false).await {
                Ok(t) => {
                    let client = me.client_for_url(&t.url).clone();
                    download_into(client, t.url, vc2, token).await
                }
                Err(e) => {
                    log::warn!("[radio] cache resolve {id} failed: {e}");
                    // download_into not started; mark failed via a no-op cache (the
                    // VideoCache stays incomplete → passthrough handles it).
                }
            }
        });
        vc
    }

    /// Proxies + repairs a livestream's DASH manifest. (Go `serveLiveManifest`.)
    async fn serve_live_manifest(self: &Arc<Self>, video_id: &str) -> Response {
        // Briefly-cached rewritten body (dashdemux refetches every ~2s).
        {
            let bodies = self.live_dash_body.lock();
            if let Some(c) = bodies.get(video_id) {
                if c.at.elapsed() < DASH_BODY_TTL {
                    println!("[Proxy] serve_live_manifest: serving cached manifest for {}", video_id);
                    return dash_response(c.body.clone());
                }
            }
        }
        let upstream = self.live_dash.lock().get(video_id).cloned();
        let upstream_url = match upstream {
            Some(url) => url,
            None => {
                println!("[Proxy] serve_live_manifest: cache missed or cleared due to 403, re-resolving...");
                match self.reresolve_dash(video_id).await {
                    Some(fresh) => fresh,
                    None => {
                        println!("[Proxy] serve_live_manifest: re-resolve failed");
                        return error_response(StatusCode::BAD_GATEWAY, "manifest unavailable");
                    }
                }
            }
        };

        println!("[Proxy] serve_live_manifest: fetching fresh manifest from upstream: {}", upstream_url);
        let body = match self.fetch_manifest(&upstream_url).await {
            Some(b) => b,
            None => {
                println!("[Proxy] serve_live_manifest: fetch_manifest failed, re-resolving...");
                // Upstream manifest URLs expire (~6h) — re-resolve once and retry.
                match self.reresolve_dash(video_id).await {
                    Some(fresh) => {
                        println!("[Proxy] serve_live_manifest: re-resolved fresh manifest: {}", fresh);
                        match self.fetch_manifest(&fresh).await {
                            Some(b) => b,
                            None => {
                                println!("[Proxy] serve_live_manifest: fetch_manifest after re-resolve failed");
                                return error_response(StatusCode::BAD_GATEWAY, "manifest unavailable");
                            }
                        }
                    }
                    None => {
                        println!("[Proxy] serve_live_manifest: re-resolve failed");
                        return error_response(StatusCode::BAD_GATEWAY, "manifest unavailable");
                    }
                }
            }
        };

        let text = String::from_utf8_lossy(&body).into_owned();
        let mut fixed = match staticize_live_mpd(&text) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("[radio] live MPD rewrite failed ({e}); serving original");
                text
            }
        };
        fixed = RE_BASE_URL.replace_all(&fixed, |caps: &Captures| {
            let attrs = &caps[1];
            let url = caps[2].trim();
            let hex_url = to_hex(url.as_bytes());
            format!("<BaseURL{}>http://127.0.0.1:{}/proxy_segment/{}/</BaseURL>", attrs, self.port, hex_url)
        }).into_owned();
        let fixed = Bytes::from(fixed.into_bytes());
        self.live_dash_body.lock().insert(
            video_id.to_string(),
            ManifestBody { body: fixed.clone(), at: Instant::now() },
        );
        dash_response(fixed)
    }

    /// Fetches a manifest body, returning `None` on transport error / non-200.
    async fn fetch_manifest(&self, url: &str) -> Option<Vec<u8>> {
        let client = self.client_for_url(url);
        let is_v6 = url_prefers_ipv6(url);
        println!("[Proxy] fetch_manifest (is_v6={}): {}", is_v6, url);
        let resp = client.get(url).send().await.ok()?;
        println!("[Proxy] fetch_manifest status: {}", resp.status());
        if resp.status() != StatusCode::OK {
            return None;
        }
        resp.bytes().await.ok().map(|b| b.to_vec())
    }

    /// Re-resolves a live stream's upstream DASH URL (expiry recovery).
    async fn reresolve_dash(&self, video_id: &str) -> Option<String> {
        let options = VideoOptions {
            request_options: RequestOptions {
                client: Some(self.http.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        let video = Video::new_with_options(video_id, options).ok()?;
        let info = video.get_info().await.ok()?;
        let dash = info.dash_manifest_url?;
        if dash.is_empty() {
            return None;
        }
        self.live_dash.lock().insert(video_id.to_string(), dash.clone());
        Some(dash)
    }
}

#[async_trait]
impl StreamResolver for YtdlResolver {
    /// Returns the player-facing URL: HLS/dash-proxy as-is, VOD routed through
    /// the local `/stream` proxy. (Go `Resolve`.)
    async fn resolve(&self, video_id: &str, force_refresh: bool) -> Result<ResolvedTrack> {
        let track = self.resolve_direct(video_id, force_refresh).await?;
        if self.port == 0 || track.url.starts_with(&self.proxy_prefix()) {
            return Ok(track);
        }
        if track.is_live {
            return Ok(ResolvedTrack {
                url: format!("http://127.0.0.1:{}/hls/master?id={}", self.port, video_id.trim()),
                is_live: true,
            });
        }
        Ok(ResolvedTrack {
            url: format!("http://127.0.0.1:{}/stream?id={}", self.port, video_id.trim()),
            is_live: false,
        })
    }
}

#[async_trait]
impl PlaylistExpander for YtdlResolver {
    /// Returns the ordered video ids of a playlist (cached 6h). (Go
    /// `ExpandPlaylist`.)
    async fn expand_playlist(
        &self,
        playlist_id: &str,
        force_refresh: bool,
        cancel: CancellationToken,
    ) -> Result<Vec<String>> {
        let playlist_id = playlist_id.trim();
        if playlist_id.is_empty() {
            return Err(Error::new("radio: empty playlist id"));
        }
        if !force_refresh {
            let cache = self.playlist_cache.lock();
            if let Some(e) = cache.get(playlist_id) {
                if !e.ids.is_empty() && e.at.elapsed() < PLAYLIST_CACHE_TTL {
                    return Ok(e.ids.clone());
                }
            }
        }

        let url = format!("https://www.youtube.com/playlist?list={playlist_id}");
        let opts = PlaylistSearchOptions { fetch_all: true, ..Default::default() };
        let playlist = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::new("radio: playlist expand cancelled")),
            res = Playlist::get(url, Some(&opts)) => {
                res.map_err(|e| Error::new(format!("youtube: get playlist {playlist_id}: {e}")))?
            }
        };
        let ids: Vec<String> = playlist
            .videos
            .iter()
            .filter(|v| !v.id.is_empty())
            .map(|v| v.id.clone())
            .collect();
        if ids.is_empty() {
            return Err(Error::new(format!("youtube: playlist {playlist_id} has no playable videos")));
        }
        self.playlist_cache.lock().insert(
            playlist_id.to_string(),
            PlaylistEntry { ids: ids.clone(), at: Instant::now() },
        );
        Ok(ids)
    }
}

// ── format adapter (bridges rusty_ytdl's VideoFormat to the portable picker) ──

#[derive(Clone)]
struct FmtAdapter {
    mime: String,
    has_audio: bool,
    bitrate: u64,
}
impl From<&VideoFormat> for FmtAdapter {
    fn from(f: &VideoFormat) -> Self {
        FmtAdapter {
            mime: f.mime_type.mime.to_string(),
            has_audio: f.has_audio,
            bitrate: f.bitrate,
        }
    }
}
impl FormatLike for FmtAdapter {
    fn mime_type(&self) -> &str {
        &self.mime
    }
    fn has_audio(&self) -> bool {
        self.has_audio
    }
    fn bitrate(&self) -> u64 {
        self.bitrate
    }
}

// ── axum handlers ──

#[derive(Deserialize)]
struct IdParam {
    id: Option<String>,
}

/// `/stream` (VOD): serve from the immutable RAM buffer once cached, else pass
/// through to googlevideo. (Go `ServeHTTP` VOD path.)
async fn stream_handler(
    State(r): State<Arc<YtdlResolver>>,
    headers: HeaderMap,
    Query(q): Query<IdParam>,
) -> Response {
    let Some(id) = q.id.filter(|s| !s.is_empty()) else {
        return error_response(StatusCode::BAD_REQUEST, "missing video id");
    };
    let vc = r.get_or_start_cache(&id);
    if let Some((buf, total, ctype)) = vc.complete_snapshot() {
        return serve_from_buffer(&headers, buf, total, &ctype);
    }
    r.passthrough(&headers, &id, vc).await
}

/// `/dash` (live): repaired static manifest.
async fn dash_handler(State(r): State<Arc<YtdlResolver>>, Query(q): Query<IdParam>) -> Response {
    let Some(id) = q.id.filter(|s| !s.is_empty()) else {
        return error_response(StatusCode::BAD_REQUEST, "missing video id");
    };
    r.serve_live_manifest(&id).await
}

impl YtdlResolver {
    /// Proxies a single range request straight to googlevideo while the in-RAM
    /// copy is still downloading. (Go `passthrough`.)
    async fn passthrough(&self, headers: &HeaderMap, video_id: &str, vc: Arc<VideoCache>) -> Response {
        let track = match self.resolve_direct(video_id, false).await {
            Ok(t) => t,
            Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        };
        let client = self.client_for_url(&track.url);
        let is_v6 = url_prefers_ipv6(&track.url);
        println!("[Passthrough] Fetching VOD (is_v6={}): {}", is_v6, track.url);

        let mut req = client.get(&track.url)
            .header(header::REFERER, "https://www.youtube.com/")
            .header(header::ORIGIN, "https://www.youtube.com");
        if let Some(rng) = headers.get(header::RANGE) {
            req = req.header(header::RANGE, rng);
        }
        let upstream = match req.send().await {
            Ok(u) => u,
            Err(e) => {
                println!("[Passthrough] request error: {}", e);
                return error_response(StatusCode::BAD_GATEWAY, &e.to_string());
            }
        };
        let status = upstream.status();
        println!("[Passthrough] status response: {}", status);
        if !status.is_success() && status != StatusCode::PARTIAL_CONTENT {
            let body_text = upstream.text().await.unwrap_or_else(|_| "failed to read body".to_string());
            println!("[Passthrough] error body: {}", body_text);
            return error_response(status, &body_text);
        }

        let total = if let Some(cr) = upstream.headers().get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()) {
            cr.rsplit('/').next().and_then(|t| t.trim().parse::<i64>().ok())
        } else {
            upstream.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|t| t.trim().parse::<i64>().ok())
        }.unwrap_or(0);

        let range_h = headers
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let (start, end, _is_range) = crate::proxy::parse_range(range_h, total);

        let mut builder = Response::builder().status(status);
        for h in [header::CONTENT_TYPE, header::CONTENT_LENGTH, header::CONTENT_RANGE] {
            if let Some(v) = upstream.headers().get(&h) {
                builder = builder.header(h, v);
            }
        }
        builder = builder.header(header::ACCEPT_RANGES, "bytes");

        let (tx, rx) = tokio::sync::mpsc::channel::<std::result::Result<Bytes, std::io::Error>>(16);
        let mut upstream_stream = upstream.bytes_stream();

        tokio::spawn(async move {
            let mut bytes_written = 0;
            loop {
                // Check if the cache has completed
                if let Some((buf, _total, _ctype)) = vc.complete_snapshot() {
                    println!("[Passthrough] Cache completed! Switching to in-memory buffer at offset {}", start + bytes_written);
                    let current_pos = (start + bytes_written) as usize;
                    let end_pos = (end + 1) as usize;
                    if current_pos < buf.len() && current_pos < end_pos {
                        let slice = buf.slice(current_pos..end_pos.min(buf.len()));
                        let _ = tx.send(Ok(slice)).await;
                    }
                    break;
                }

                use futures_util::StreamExt;
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                        // Wake up periodically to check cache completion status
                    }
                    next = upstream_stream.next() => {
                        match next {
                            Some(Ok(bytes)) => {
                                let len = bytes.len() as i64;
                                if tx.send(Ok(bytes)).await.is_err() {
                                    break;
                                }
                                bytes_written += len;
                            }
                            Some(Err(e)) => {
                                println!("[Passthrough] Upstream read error: {}", e);
                                let _ = tx.send(Err(std::io::Error::other(e))).await;
                                break;
                            }
                            None => {
                                break;
                            }
                        }
                    }
                }
            }
        });

        builder
            .body(Body::from_stream(ReceiverStream(rx)))
            .unwrap_or_else(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "body error"))
    }
}

/// Answers a (possibly ranged) request from the fully-buffered immutable copy.
/// (Go `serveFromBuffer`.)
fn serve_from_buffer(headers: &HeaderMap, buf: Bytes, total: i64, ctype: &str) -> Response {
    let range_h = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let (mut start, mut end, is_range) = crate::proxy::parse_range(range_h, total);

    let mut builder = Response::builder().header(header::ACCEPT_RANGES, "bytes");
    if !ctype.is_empty() {
        builder = builder.header(header::CONTENT_TYPE, ctype);
    }
    if is_range {
        builder = builder
            .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{total}"))
            .header(header::CONTENT_LENGTH, (end - start + 1).to_string())
            .status(StatusCode::PARTIAL_CONTENT);
    } else {
        builder = builder
            .header(header::CONTENT_LENGTH, total.to_string())
            .status(StatusCode::OK);
    }

    if start < 0 {
        start = 0;
    }
    let hi = buf.len() as i64 - 1;
    if end > hi {
        end = hi;
    }
    let body = if start <= end {
        Body::from(buf.slice(start as usize..=end as usize))
    } else {
        Body::empty()
    };
    builder.body(body).unwrap_or_else(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "body error"))
}

fn dash_response(body: Bytes) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/dash+xml")
        .body(Body::from(body))
        .unwrap_or_else(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "body error"))
}

fn error_response(status: StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .body(Body::from(msg.to_string()))
        .unwrap()
}

#[derive(Deserialize)]
struct HlsSubParam {
    id: Option<String>,
    url: Option<String>,
}

async fn hls_master_handler(
    State(r): State<Arc<YtdlResolver>>,
    Query(q): Query<IdParam>,
) -> Response {
    let Some(id) = q.id.filter(|s| !s.is_empty()) else {
        return error_response(StatusCode::BAD_REQUEST, "missing video id");
    };
    let track = match r.resolve_direct(&id, false).await {
        Ok(t) => t,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let client = r.client_for_url(&track.url);
    let resp = match client.get(&track.url)
        .header(header::REFERER, "https://www.youtube.com/")
        .header(header::ORIGIN, "https://www.youtube.com")
        .send().await {
        Ok(res) => res,
        Err(e) => return error_response(StatusCode::BAD_GATEWAY, &e.to_string()),
    };
    if resp.status() != StatusCode::OK {
        return error_response(resp.status(), "failed to fetch HLS master manifest");
    }
    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let mut new_lines = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            let hex_url = to_hex(trimmed.as_bytes());
            new_lines.push(format!("http://127.0.0.1:{}/hls/sub?id={}&url={}", r.port, id, hex_url));
        } else {
            new_lines.push(line.to_string());
        }
    }
    let rewritten = new_lines.join("\n");

    Response::builder()
        .header(header::CONTENT_TYPE, "application/x-mpegURL")
        .body(Body::from(rewritten))
        .unwrap_or_else(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "body error"))
}

async fn hls_sub_handler(
    State(r): State<Arc<YtdlResolver>>,
    Query(q): Query<HlsSubParam>,
) -> Response {
    let Some(_id) = q.id.filter(|s| !s.is_empty()) else {
        return error_response(StatusCode::BAD_REQUEST, "missing video id");
    };
    let Some(url_hex) = q.url.filter(|s| !s.is_empty()) else {
        return error_response(StatusCode::BAD_REQUEST, "missing sub-playlist url");
    };
    let url_bytes = match from_hex(&url_hex) {
        Some(b) => b,
        None => return error_response(StatusCode::BAD_REQUEST, "invalid hex url"),
    };
    let real_url = match String::from_utf8(url_bytes) {
        Ok(u) => u,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid UTF-8 in url"),
    };

    let client = r.client_for_url(&real_url);
    let resp = match client.get(&real_url)
        .header(header::REFERER, "https://www.youtube.com/")
        .header(header::ORIGIN, "https://www.youtube.com")
        .send().await {
        Ok(res) => res,
        Err(e) => return error_response(StatusCode::BAD_GATEWAY, &e.to_string()),
    };
    if resp.status() != StatusCode::OK {
        return error_response(resp.status(), "failed to fetch HLS sub-playlist");
    }
    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let mut new_lines = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            let hex_url = to_hex(trimmed.as_bytes());
            new_lines.push(format!("http://127.0.0.1:{}/proxy_segment/{}", r.port, hex_url));
        } else {
            new_lines.push(line.to_string());
        }
    }
    let rewritten = new_lines.join("\n");

    Response::builder()
        .header(header::CONTENT_TYPE, "application/x-mpegURL")
        .body(Body::from(rewritten))
        .unwrap_or_else(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "body error"))
}

async fn proxy_segment_handler(
    State(r): State<Arc<YtdlResolver>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    AxumPath(path): AxumPath<String>,
) -> Response {
    let (hex_base, relative_suffix) = if let Some(slash_idx) = path.find('/') {
        (&path[..slash_idx], Some(&path[slash_idx..]))
    } else {
        (path.as_str(), None)
    };

    let original_base_bytes = match from_hex(hex_base) {
        Some(b) => b,
        None => return error_response(StatusCode::BAD_REQUEST, "invalid hex URL"),
    };
    let original_base_url = match String::from_utf8(original_base_bytes) {
        Ok(s) => s,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid UTF-8 in URL"),
    };

    let mut final_url = original_base_url;
    if let Some(suffix) = relative_suffix {
        if final_url.ends_with('/') && suffix.starts_with('/') {
            final_url.push_str(&suffix[1..]);
        } else {
            final_url.push_str(suffix);
        }
    }

    if let Some(query) = uri.query() {
        if final_url.contains('?') {
            final_url.push('&');
        } else {
            final_url.push('?');
        }
        final_url.push_str(query);
    }

    r.proxy_segment(&headers, &final_url).await
}

impl YtdlResolver {
    fn client_for_url(&self, url: &str) -> reqwest::Client {
        if let Some(ip) = parse_url_ip(url) {
            println!("[Proxy] client_for_url: parsed IP {} from URL", ip);
            if ip.is_ipv6() {
                let mut map = self.bound_clients.lock();
                if let Some(client) = map.get(&ip) {
                    println!("[Proxy] client_for_url: returning cached client for local IPv6 address {}", ip);
                    return client.clone();
                }
                match reqwest::Client::builder()
                    .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
                    .local_address(ip)
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                {
                    Ok(client) => {
                        println!("[Proxy] client_for_url: successfully created and cached client bound to local IPv6 address {}", ip);
                        map.insert(ip, client.clone());
                        return client;
                    }
                    Err(e) => {
                        println!("[Proxy] client_for_url: failed to bind client to local IPv6 address {}: {}", ip, e);
                    }
                }
            } else {
                println!("[Proxy] client_for_url: parsed IP is IPv4, not binding to it");
            }
        } else {
            println!("[Proxy] client_for_url: no IP parsed from URL");
        }
        if url_prefers_ipv6(url) {
            println!("[Proxy] client_for_url: URL prefers IPv6, using default http_v6 client");
            self.http_v6.clone()
        } else {
            println!("[Proxy] client_for_url: URL prefers IPv4/other, using default http_v4 client");
            self.http_v4.clone()
        }
    }

    async fn proxy_segment(self: &Arc<Self>, _headers: &HeaderMap, target_url: &str) -> Response {
        // Trigger prefetch for the NEXT segments (N+1 and N+2)
        self.trigger_prefetch(target_url);

        match self.fetch_or_await_segment(target_url, false).await {
            Ok(body_bytes) => {
                let mut builder = Response::builder().status(StatusCode::OK);
                builder = builder.header(header::CONTENT_TYPE, "audio/mp4");
                builder = builder.header(header::ACCEPT_RANGES, "bytes");
                builder = builder.header(header::CONTENT_LENGTH, body_bytes.len().to_string());
                builder.body(Body::from(body_bytes)).unwrap()
            }
            Err(e) => {
                error_response(StatusCode::INTERNAL_SERVER_ERROR, &e)
            }
        }
    }

    /// Records that the CDN served `url` (HTTP 200/206), advancing the per-video
    /// observed served edge that bounds the prefetch probe.
    fn note_served_sq(&self, url: &str) {
        let sq = RE_SQ.captures(url)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<i64>().ok());
        if let (Some(vid), Some(sq)) = (parse_video_id(url), sq) {
            let mut m = self.max_served_sq.lock();
            let e = m.entry(vid).or_insert(sq);
            if sq > *e {
                *e = sq;
            }
        }
    }

    fn trigger_prefetch(self: &Arc<Self>, url: &str) {
        let Some(caps) = RE_SQ.captures(url) else {
            return;
        };
        let Some(sq_match) = caps.get(1) else {
            return;
        };
        let Ok(sq_num) = sq_match.as_str().parse::<i64>() else {
            return;
        };

        // Probe ceiling: don't prefetch more than a few segments past the observed
        // served edge — those are not published yet, so they only 403-storm
        // googlevideo (which then throttles the IP, 403ing the *real* segments too).
        let probe_margin: i64 = std::env::var("CLAWD_PREFETCH_PROBE").ok()
            .and_then(|s| s.parse().ok()).unwrap_or(PREFETCH_PROBE_MARGIN);
        let probe_ceil = parse_video_id(url)
            .and_then(|vid| self.max_served_sq.lock().get(&vid).copied())
            .map(|served| served + probe_margin);

        // Maintain a deep readahead window so the proxy keeps the next ~80s of
        // segments warm in RAM. adaptivedemux2 buffers in bursts: it fills its
        // buffer, stops downloading, drains it to 0%, then refills. A shallow +2
        // prefetch leaves the proxy cold during the ~50s drain, so the refill burst
        // hits (often rate-limited) googlevideo and the buffer can't refill before
        // audio runs out — the periodic ~1-min stall. A window deeper than one
        // buffer cycle means the last fill-burst request pre-warms the *next* refill
        // during the idle drain, so refills are served from RAM. Bounded upstream
        // concurrency + live-priority acquisition (see UPSTREAM_SEM) keep the deep
        // window from starving the on-demand request or stampeding googlevideo.
        let depth: i64 = std::env::var("CLAWD_PREFETCH_DEPTH").ok()
            .and_then(|s| s.parse().ok()).unwrap_or(READAHEAD_DEPTH);
        for offset in 1..=depth {
            let next_sq = sq_num + offset;
            if let Some(ceil) = probe_ceil {
                if next_sq > ceil {
                    break;
                }
            }
            let next_url = url.replace(&format!("/sq/{}/", sq_num), &format!("/sq/{}/", next_sq));

            if self.segment_cache.lock().contains_key(&next_url) {
                continue;
            }

            let me = Arc::clone(self);
            self.rt.spawn(async move {
                let _ = me.fetch_or_await_segment(&next_url, true).await;
            });
        }
    }

    async fn fetch_or_await_segment(self: &Arc<Self>, url: &str, is_prefetch: bool) -> std::result::Result<Bytes, String> {
        // Clean up expired segments. The TTL must outlive the readahead horizon
        // (depth × ~2s ≈ 80s) plus the time until playback reaches the segment
        // (~one buffer cycle), or deep-prefetched segments get evicted before use.
        {
            let ttl: u64 = std::env::var("CLAWD_SEG_TTL_S").ok()
                .and_then(|s| s.parse().ok()).unwrap_or(SEGMENT_CACHE_TTL_S);
            let mut cache = self.segment_cache.lock();
            cache.retain(|_, entry| entry.at.elapsed() < Duration::from_secs(ttl));
        }

        loop {
            let state_opt = self.segment_cache.lock().get(url).map(|e| e.state.clone());

            match state_opt {
                Some(SegmentState::Complete(bytes)) => {
                    println!("[ProxySegment] Cache hit for complete segment: {}", url);
                    self.note_served_sq(url);
                    return Ok(bytes);
                }
                Some(SegmentState::InFlight(mut rx)) => {
                    println!("[ProxySegment] Request in-flight, awaiting completion for (is_prefetch={}): {}", is_prefetch, url);
                    let mut success_bytes = None;
                    loop {
                        {
                            let status = rx.borrow();
                            match &*status {
                                SegmentStatus::Done(bytes) => {
                                    println!("[ProxySegment] In-flight request completed successfully: {}", url);
                                    success_bytes = Some(bytes.clone());
                                    break;
                                }
                                SegmentStatus::Failed => {
                                    println!("[ProxySegment] In-flight request failed: {}", url);
                                    break;
                                }
                                SegmentStatus::Pending => {}
                            }
                        }
                        if rx.changed().await.is_err() {
                            println!("[ProxySegment] In-flight request watch channel closed: {}", url);
                            break;
                        }
                    }
                    if let Some(bytes) = success_bytes {
                        return Ok(bytes);
                    }
                    if !is_prefetch {
                        println!("[ProxySegment] In-flight request failed, caller is player. Removing from cache and retrying upstream fetch directly.");
                        {
                            let mut cache = self.segment_cache.lock();
                            let remove_entry = if let Some(entry) = cache.get(url) {
                                match &entry.state {
                                    SegmentState::InFlight(entry_rx) => {
                                        entry_rx.same_channel(&rx)
                                    }
                                    _ => false,
                                }
                            } else {
                                false
                            };
                            if remove_entry {
                                cache.remove(url);
                            }
                        }
                        continue;
                    } else {
                        return Err("in-flight request failed".to_string());
                    }
                }
                None => {
                    // Initialize in-flight state
                    let (tx, rx) = tokio::sync::watch::channel(SegmentStatus::Pending);
                    self.segment_cache.lock().insert(
                        url.to_string(),
                        SegmentEntry {
                            state: SegmentState::InFlight(rx),
                            at: Instant::now(),
                        },
                    );

                    println!("[ProxySegment] Cache miss (is_prefetch={}), fetching segment from upstream: {}", is_prefetch, url);
                    // Acquire an upstream slot. The on-demand player request (the
                    // segment GStreamer needs NOW) acquires blocking — it has
                    // priority. Readahead prefetch yields to it (try-acquire with
                    // backoff), so a deep readahead can never queue ahead of, and
                    // starve, the live request. Prefetch that can't get a slot
                    // within the window simply gives up (it will be retried on the
                    // next request, or fetched on demand).
                    let _permit = if is_prefetch {
                        let mut got = None;
                        for _ in 0..200 {
                            if let Ok(p) = UPSTREAM_SEM.try_acquire() {
                                got = Some(p);
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        match got {
                            Some(p) => p,
                            None => {
                                self.segment_cache.lock().remove(url);
                                let _ = tx.send(SegmentStatus::Failed);
                                return Err("prefetch upstream slot timeout".to_string());
                            }
                        }
                    } else {
                        UPSTREAM_SEM.acquire().await.unwrap()
                    };
                    let client = self.client_for_url(url);
                    // On-demand (player) 403: the live-edge segment is announced in the
                    // manifest but the CDN hasn't started serving it yet. Returning the
                    // 403 to dashdemux makes souphttpsrc treat it as FATAL → pipeline
                    // restart; the old fix (clear manifest + re-resolve) handed dashdemux
                    // a forward-shifted window = the "jump to live". Instead ABSORB it in
                    // the proxy: retry the SAME url (same host) with backoff until the
                    // segment publishes (~one segment duration), capped under souphttpsrc's
                    // ~15s read timeout, touching nothing. The deep readahead keeps the
                    // buffer warm so the brief wait is usually hidden. Only a 403 that
                    // persists past the budget falls back to the (rare) manifest re-resolve
                    // — still better than a hard pipeline restart. Prefetch never blocks.
                    let edge_retries: u32 = std::env::var("CLAWD_EDGE_403_RETRIES").ok()
                        .and_then(|s| s.parse().ok()).unwrap_or(18);
                    let edge_backoff_ms: u64 = std::env::var("CLAWD_EDGE_403_BACKOFF_MS").ok()
                        .and_then(|s| s.parse().ok()).unwrap_or(700);
                    let mut attempt: u32 = 0;
                    let body_bytes = loop {
                        let upstream = match client.get(url)
                            .header(header::REFERER, "https://www.youtube.com/")
                            .header(header::ORIGIN, "https://www.youtube.com")
                            .send().await
                        {
                            Ok(u) => u,
                            Err(e) => {
                                println!("[ProxySegment] upstream send error: {}", e);
                                let _ = tx.send(SegmentStatus::Failed);
                                self.segment_cache.lock().remove(url);
                                return Err(e.to_string());
                            }
                        };

                        let status = upstream.status();
                        println!("[ProxySegment] upstream status response: {}", status);

                        // Transient live-edge 403 on a player request → wait for the
                        // segment to publish and retry; do NOT touch the manifest.
                        if status == StatusCode::FORBIDDEN && !is_prefetch && attempt < edge_retries {
                            attempt += 1;
                            println!("[ProxySegment] live-edge 403 (not-yet-served) for {}, retry {}/{} in {}ms (proxy-absorbed, NOT re-resolving)",
                                url, attempt, edge_retries, edge_backoff_ms);
                            tokio::time::sleep(Duration::from_millis(edge_backoff_ms)).await;
                            continue;
                        }

                        if status == StatusCode::FORBIDDEN {
                            if !is_prefetch {
                                // Past the retry budget → persistent; last-resort
                                // re-resolve (rare jump, still beats a fatal restart).
                                if let Some(video_id) = parse_video_id(url) {
                                    println!("[ProxySegment] persistent 403 for video_id: {} after {} retries, re-resolving manifest", video_id, attempt);
                                    self.live_dash.lock().remove(&video_id);
                                    self.live_dash_body.lock().remove(&video_id);
                                }
                            } else {
                                println!("[ProxySegment] Prefetch got 403 Forbidden for: {} (not clearing manifest cache)", url);
                            }
                        }
                        if !status.is_success() && status != StatusCode::PARTIAL_CONTENT {
                            let body_text = upstream.text().await.unwrap_or_else(|_| "failed to read body".to_string());
                            println!("[ProxySegment] upstream error body (after {attempt} retries): {}", body_text);
                            let _ = tx.send(SegmentStatus::Failed);
                            self.segment_cache.lock().remove(url);
                            return Err(format!("upstream returned error status: {}", status));
                        }

                        match upstream.bytes().await {
                            Ok(b) => break b,
                            Err(e) => {
                                println!("[ProxySegment] upstream bytes read error: {}", e);
                                let _ = tx.send(SegmentStatus::Failed);
                                self.segment_cache.lock().remove(url);
                                return Err(e.to_string());
                            }
                        }
                    };

                    // Store as completed
                    {
                        let mut cache = self.segment_cache.lock();
                        cache.insert(
                            url.to_string(),
                            SegmentEntry {
                                state: SegmentState::Complete(body_bytes.clone()),
                                at: Instant::now(),
                            },
                        );
                    }

                    // Notify all listeners
                    let _ = tx.send(SegmentStatus::Done(body_bytes.clone()));
                    self.note_served_sq(url);
                    return Ok(body_bytes);
                }
            }
        }
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn from_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        let b = u8::from_str_radix(&hex[i..i + 2], 16).ok()?;
        bytes.push(b);
    }
    Some(bytes)
}

fn url_prefers_ipv6(url: &str) -> bool {
    let ip_str = if let Some(pos) = url.find("/ip/") {
        &url[pos + 4..]
    } else if let Some(pos) = url.find("ip=") {
        &url[pos + 3..]
    } else {
        return false;
    };
    let end = ip_str.find(['/', '&']).unwrap_or(ip_str.len());
    let ip = &ip_str[..end];
    ip.contains(':') || ip.contains("%3A") || ip.contains("%3a")
}

fn parse_url_ip(url: &str) -> Option<std::net::IpAddr> {
    let ip_str = if let Some(pos) = url.find("/ip/") {
        &url[pos + 4..]
    } else if let Some(pos) = url.find("ip=") {
        &url[pos + 3..]
    } else {
        return None;
    };
    let end = ip_str.find(['/', '&']).unwrap_or(ip_str.len());
    let ip_raw = &ip_str[..end];
    let decoded = ip_raw.replace("%3A", ":").replace("%3a", ":");
    decoded.parse::<std::net::IpAddr>().ok()
}

fn parse_video_id(url: &str) -> Option<String> {
    let id_str = if let Some(pos) = url.find("/id/") {
        &url[pos + 4..]
    } else if let Some(pos) = url.find("id=") {
        &url[pos + 3..]
    } else {
        return None;
    };
    let end = id_str.find(['/', '&']).unwrap_or(id_str.len());
    let raw_id = &id_str[..end];
    let dot_end = raw_id.find('.').unwrap_or(raw_id.len());
    Some(raw_id[..dot_end].to_string())
}

struct ReceiverStream(tokio::sync::mpsc::Receiver<std::result::Result<Bytes, std::io::Error>>);

impl futures_util::Stream for ReceiverStream {
    type Item = std::result::Result<Bytes, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn test_fetch_segment() {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let _resolver = YtdlResolver::new(tokio::runtime::Handle::current()).unwrap();
            let video = Video::new("X4VbdwhkE10").unwrap();
            let info = video.get_info().await.unwrap();
            println!("Parsed DASH Manifest URL: {:?}", info.dash_manifest_url);
            println!("Parsed HLS Manifest URL: {:?}", info.hls_manifest_url);
            
            let hls_url = info.hls_manifest_url.clone().expect("HLS manifest URL not found");
            println!("HLS URL: {}", hls_url);
            
            // Fetch the HLS master manifest
            let client = reqwest::Client::new();
            let resp = client.get(&hls_url)
                .header("Referer", "https://www.youtube.com/")
                .header("Origin", "https://www.youtube.com")
                .send()
                .await
                .unwrap();
            let manifest_text = resp.text().await.unwrap();
            
            // Extract the first sub-playlist URL
            let mut sub_url = String::new();
            for line in manifest_text.lines() {
                if line.starts_with("https://") {
                    sub_url = line.to_string();
                    break;
                }
            }
            assert!(!sub_url.is_empty(), "No sub-playlist URL found");
            println!("Sub URL: {}", sub_url);
            
            // Fetch the sub-playlist
            let resp = client.get(&sub_url)
                .header("Referer", "https://www.youtube.com/")
                .header("Origin", "https://www.youtube.com")
                .send()
                .await
                .unwrap();
            let sub_manifest_text = resp.text().await.unwrap();
            
            // Extract the first segment URL
            let mut segment_url = String::new();
            for line in sub_manifest_text.lines() {
                if line.starts_with("https://") {
                    segment_url = line.to_string();
                    break;
                }
            }
            assert!(!segment_url.is_empty(), "No segment URL found");
            println!("Segment URL: {}", segment_url);
            
            // Let's try fetching the segment using different local IP bindings and User-Agents
            let ip_addresses = vec![
                None, // default
                Some("2600:8805:1a00:3270::3901"),
                Some("2600:8805:1a00:3270:d1f:c271:b7ad:fa91"),
            ];
            
            let user_agents = vec![
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/70.0.3513.0 Safari/537.36",
                "com.google.ios.youtube/19.29.1 (iPhone16,2; U; CPU iOS 17_5_1 like Mac OS X;)",
            ];
            
            for ip_str in ip_addresses {
                for &ua in &user_agents {
                    let mut builder = reqwest::Client::builder().user_agent(ua);
                    if let Some(ip) = ip_str {
                        if let Ok(addr) = ip.parse::<std::net::IpAddr>() {
                            builder = builder.local_address(addr);
                        }
                    }
                    
                    let test_client = match builder.build() {
                        Ok(c) => c,
                        Err(e) => {
                            println!("Failed to build client for IP {:?}: {}", ip_str, e);
                            continue;
                        }
                    };
                    
                    let res = test_client.get(&segment_url)
                        .header("Referer", "https://www.youtube.com/")
                        .header("Origin", "https://www.youtube.com")
                        .send()
                        .await;
                    
                    match res {
                        Ok(r) => {
                            println!("With IP {:?}, UA {}: Status {}", ip_str, &ua[..24], r.status());
                            if r.status() == 403 {
                                println!("  Headers:");
                                for (name, value) in r.headers() {
                                    println!("    {}: {:?}", name, value);
                                }
                            }
                        }
                        Err(e) => {
                            println!("With IP {:?}, UA {}: Error {}", ip_str, &ua[..24], e);
                        }
                    }
                }
            }
        });
    }

    /// Network probe (Go `probe_test.go::TestProbeResolution`). `#[ignore]`d so
    /// the offline gate stays green — YouTube extraction is flaky and the spine
    /// is validated by the deterministic unit tests. Run with:
    /// `cargo test -p clawdpanel-media --ignored -- --nocapture`.
    #[test]
    #[ignore = "network: dump raw formats for debugging"]
    fn dump_formats() {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let video = Video::new("X4VbdwhkE10").unwrap();
            match video.get_info().await {
                Ok(info) => {
                    eprintln!("dash={:?} hls={:?} formats={}", info.dash_manifest_url.as_ref(), info.hls_manifest_url.as_ref(), info.formats.len());
                    if let Some(hls_url) = info.hls_manifest_url {
                        let client = reqwest::Client::new();
                        let resp = client.get(&hls_url).send().await.unwrap();
                        let text = resp.text().await.unwrap();
                        // Find the first URL in the manifest
                        if let Some(pos) = text.find("https://") {
                            let url_end = text[pos..].find('\n').unwrap_or(text[pos..].len());
                            let sub_url = text[pos..pos + url_end].trim();
                            eprintln!("Sub-playlist URL: {}", sub_url);
                            let sub_resp = client.get(sub_url).send().await.unwrap();
                            let sub_text = sub_resp.text().await.unwrap();
                            eprintln!("Sub-playlist Content:\n{}", sub_text);
                        }
                    }
                }
                Err(e) => eprintln!("get_info error: {e}"),
            }
        });
    }

    /// Network probe (Go `probe_test.go::TestProbeResolution`). `#[ignore]`d so
    /// the offline gate stays green. Asserts the resolver returns a proxy URL
    /// (resolution works); the actual byte fetch is logged, not asserted —
    /// rusty_ytdl 0.7.4 leaves adaptive-format URLs ciphered and the itag-18
    /// fallback URL needs the `n`-throttling transform / POToken googlevideo
    /// rejects without (403). See the S7 extraction follow-up issue.
    #[test]
    #[ignore = "network: live YouTube extraction (rusty_ytdl)"]
    fn probe_resolution_vod() {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let resolver = YtdlResolver::new(tokio::runtime::Handle::current()).unwrap();
            // Tycho VOD (same probe id as the Go test).
            let track = resolver.resolve("BGXOYfZMR0w", true).await.expect("resolve");
            eprintln!("resolved (is_live={}): {}", track.is_live, track.url);
            assert!(track.url.contains("/stream?id="), "VOD should route through the proxy");

            // Pull the first KiB through the proxy → exercises get_or_start + passthrough.
            let resp = reqwest::Client::new()
                .get(&track.url)
                .header(reqwest::header::RANGE, "bytes=0-1023")
                .send()
                .await
                .expect("proxy GET");
            eprintln!("proxy status: {} (403 = known extraction gap)", resp.status());

            // Wait and monitor background download
            for i in 0..15 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let cached = resolver.byte_cache.lock().get("BGXOYfZMR0w");
                if let Some(vc) = cached {
                    if let Some((_buf, total, ctype)) = vc.complete_snapshot() {
                        eprintln!("Download complete after {}s! total={}, ctype={}", i + 1, total, ctype);
                        return;
                    }
                }
            }
            panic!("Download did not complete in 15 seconds!");
        });
    }

    #[tokio::test]
    #[ignore]
    async fn test_playlist_expand_live() {
        let url = "https://www.youtube.com/playlist?list=PLLvWV__Bn2_PwR92FfrxjsZCAM7zyxzze";
        let opts = PlaylistSearchOptions { fetch_all: true, ..Default::default() };
        let res = Playlist::get(url, Some(&opts)).await;
        match res {
            Ok(playlist) => {
                println!("Playlist ID: {}", playlist.id);
                println!("Playlist Name: {}", playlist.name);
                println!("Videos count: {}", playlist.videos.len());
                for video in playlist.videos.iter().take(5) {
                    println!("  Video: {} - {}", video.id, video.title);
                }
            }
            Err(e) => {
                panic!("Playlist::get failed: {:?}", e);
            }
        }
    }

    #[tokio::test]
    #[ignore = "network: live YouTube extraction; hardcoded live id rotates"]
    async fn test_print_staticized_manifest() {
        let resolver = YtdlResolver::new(tokio::runtime::Handle::current()).unwrap();
        // A LOFI Girl-style live id (rotates over time; jfKfPfyJRdk ended 2026-05).
        let track = resolver.resolve("X4VbdwhkE10", true).await.unwrap();
        println!("Resolved track URL: {}", track.url);

        let client = reqwest::Client::new();
        let resp = client.get(&track.url).send().await.unwrap();
        let body = resp.text().await.unwrap();
        println!("Staticized Manifest Body:\n{}", body);
    }
}
