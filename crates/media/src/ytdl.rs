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
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use parking_lot::Mutex;
use serde::Deserialize;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

use rusty_ytdl::search::{Playlist, PlaylistSearchOptions};
use rusty_ytdl::{Video, VideoFormat};

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
        let resolver = Arc::new(YtdlResolver {
            rt: rt.clone(),
            http,
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

        let video = Video::new(video_id)
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
                Ok(t) => download_into(me.http.clone(), t.url, vc2, token).await,
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

        let mut body = match self.fetch_manifest(&upstream).await {
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
        let fixed = match staticize_live_mpd(&text) {
            Ok(f) => f.into_bytes(),
            Err(e) => {
                log::warn!("[radio] live MPD rewrite failed ({e}); serving original");
                std::mem::take(&mut body)
            }
        };
        let fixed = Bytes::from(fixed);
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
        let video = Video::new(video_id).ok()?;
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
        // Live, proxy down, or already a proxy URL (live /dash) → play directly.
        if track.is_live || self.port == 0 || track.url.starts_with(&self.proxy_prefix()) {
            return Ok(track);
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
        let mut req = self.http.get(&track.url);
        if let Some(rng) = headers.get(header::RANGE) {
            req = req.header(header::RANGE, rng);
        }
        let upstream = match req.send().await {
            Ok(u) => u,
            Err(e) => return error_response(StatusCode::BAD_GATEWAY, &e.to_string()),
        };
        let status = upstream.status();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Network probe (Go `probe_test.go::TestProbeResolution`). `#[ignore]`d so
    /// the offline gate stays green — YouTube extraction is flaky and the spine
    /// is validated by the deterministic unit tests. Run with:
    /// `cargo test -p clawdpanel-media --ignored -- --nocapture`.
    #[test]
    #[ignore = "network: dump raw formats for debugging"]
    fn dump_formats() {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let video = Video::new("BGXOYfZMR0w").unwrap();
            match video.get_info().await {
                Ok(info) => {
                    eprintln!("dash={:?} hls={:?} formats={}", info.dash_manifest_url.is_some(), info.hls_manifest_url.is_some(), info.formats.len());
                    for f in &info.formats {
                        eprintln!("itag={} mime={} audio={} br={} url_len={}", f.itag, f.mime_type.mime, f.has_audio, f.bitrate, f.url.len());
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
}
