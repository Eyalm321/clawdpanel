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

static RE_BASE_URL: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?s)<BaseURL\b([^>]*)>(.*?)</BaseURL>"#).unwrap());

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
            .build()
            .unwrap_or_default();
        let http_v4 = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
            .local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
            .build()
            .unwrap_or_default();
        let http_v6 = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
            .local_address(std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED))
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
                    return dash_response(c.body.clone());
                }
            }
        }
        let upstream = self.live_dash.lock().get(video_id).cloned();
        let Some(upstream) = upstream else {
            return error_response(StatusCode::NOT_FOUND, "unknown live stream");
        };

        let body = match self.fetch_manifest(&upstream).await {
            Some(b) => b,
            None => {
                // Upstream manifest URLs expire (~6h) — re-resolve once and retry.
                match self.reresolve_dash(video_id).await {
                    Some(fresh) => match self.fetch_manifest(&fresh).await {
                        Some(b) => b,
                        None => return error_response(StatusCode::BAD_GATEWAY, "manifest unavailable"),
                    },
                    None => return error_response(StatusCode::BAD_GATEWAY, "manifest unavailable"),
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
        let resp = self.http.get(url).send().await.ok()?;
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
    r.passthrough(&headers, &id).await
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
    async fn passthrough(&self, headers: &HeaderMap, video_id: &str) -> Response {
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

        let mut builder = Response::builder().status(status);
        for h in [header::CONTENT_TYPE, header::CONTENT_LENGTH, header::CONTENT_RANGE] {
            if let Some(v) = upstream.headers().get(&h) {
                builder = builder.header(h, v);
            }
        }
        builder = builder.header(header::ACCEPT_RANGES, "bytes");
        builder
            .body(Body::from_stream(upstream.bytes_stream()))
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
    let resp = match r.http.get(&track.url)
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

    let resp = match r.http.get(&real_url)
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
            if let Ok(client) = reqwest::Client::builder()
                .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
                .local_address(ip)
                .build()
            {
                return client;
            }
        }
        if url_prefers_ipv6(url) {
            self.http_v6.clone()
        } else {
            self.http_v4.clone()
        }
    }

    async fn proxy_segment(&self, headers: &HeaderMap, target_url: &str) -> Response {
        let client = self.client_for_url(target_url);
        let is_v6 = url_prefers_ipv6(target_url);
        println!("[ProxySegment] Fetching segment (is_v6={}): {}", is_v6, target_url);

        let mut req = client.get(target_url)
            .header(header::REFERER, "https://www.youtube.com/")
            .header(header::ORIGIN, "https://www.youtube.com");
        if let Some(rng) = headers.get(header::RANGE) {
            req = req.header(header::RANGE, rng);
        }
        let upstream = match req.send().await {
            Ok(u) => u,
            Err(e) => {
                println!("[ProxySegment] request error: {}", e);
                return error_response(StatusCode::BAD_GATEWAY, &e.to_string());
            }
        };
        let status = upstream.status();
        println!("[ProxySegment] status response: {}", status);
        if !status.is_success() && status != StatusCode::PARTIAL_CONTENT {
            let body_text = upstream.text().await.unwrap_or_else(|_| "failed to read body".to_string());
            println!("[ProxySegment] error body: {}", body_text);
            return error_response(status, &body_text);
        }

        let mut builder = Response::builder().status(status);
        for h in [header::CONTENT_TYPE, header::CONTENT_LENGTH, header::CONTENT_RANGE] {
            if let Some(v) = upstream.headers().get(&h) {
                builder = builder.header(h, v);
            }
        }
        builder = builder.header(header::ACCEPT_RANGES, "bytes");
        builder
            .body(Body::from_stream(upstream.bytes_stream()))
            .unwrap_or_else(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "body error"))
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn from_hex(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
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
    let end = ip_str.find(|c| c == '/' || c == '&').unwrap_or(ip_str.len());
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
    let end = ip_str.find(|c| c == '/' || c == '&').unwrap_or(ip_str.len());
    let ip_raw = &ip_str[..end];
    let decoded = ip_raw.replace("%3A", ":").replace("%3a", ":");
    decoded.parse::<std::net::IpAddr>().ok()
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
}
