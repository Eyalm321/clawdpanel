//! Range parsing + the in-RAM VOD byte-cache — the mechanical, network-free
//! core of Go's `radio.go` proxy (`parseRange`, `videoCache`, `downloadInto`,
//! `serveFromBuffer`). The HTTP server + extraction live in [`crate::ytdl`],
//! which owns these pieces; the pieces are split out so `parse_range` is
//! unit-tested without touching the network.

use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use parking_lot::Mutex;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Download tuning (Go constants). YouTube throttles a single connection to
/// ~playback rate, so disjoint segments fetched concurrently multiply throughput
/// and let even a long mix finish caching in seconds — after which seeks are
/// instant.
const DL_SEGMENTS: i64 = 8;
const DL_MIN_SEGMENT: i64 = 1 << 20; // 1 MiB: don't over-split small files
pub const DL_START_DELAY_MS: u64 = 1500; // playback head start before the fan-out

/// Parses a single `bytes=start-end` header against the known total. Only the
/// first range is honoured. Returns `is_range=false` for an absent/unparseable
/// header (caller serves a 200). 1:1 port of Go `parseRange`.
pub fn parse_range(h: &str, total: i64) -> (i64, i64, bool) {
    let end_default = total - 1;
    let Some(spec) = h.strip_prefix("bytes=") else {
        return (0, end_default, false);
    };
    let spec = spec.split(',').next().unwrap_or(spec);
    let Some(dash) = spec.find('-') else {
        return (0, end_default, false);
    };
    let (start_str, end_str) = (&spec[..dash], &spec[dash + 1..]);
    if start_str.is_empty() {
        // suffix range: bytes=-N (last N bytes)
        if let Ok(n) = end_str.parse::<i64>() {
            if n > 0 {
                let start = (total - n).max(0);
                return (start, total - 1, true);
            }
        }
        return (0, end_default, false);
    }
    let Ok(start) = start_str.parse::<i64>() else {
        return (0, end_default, false);
    };
    let mut end = end_default;
    if !end_str.is_empty() {
        if let Ok(e) = end_str.parse::<i64>() {
            end = e;
        }
    }
    if end >= total {
        end = total - 1;
    }
    let start = start.min(end);
    (start, end, true)
}

/// An in-memory copy of one VOD's audio stream, filled once by a background
/// download. Served to the player only after the download completes; the buffer
/// is immutable thereafter (an immutable [`Bytes`]). (Go `videoCache`.)
pub struct VideoCache {
    state: Mutex<CacheState>,
    pub cancel: CancellationToken,
}

struct CacheState {
    total: i64,
    content_type: String,
    complete: bool,
    err: bool,
    buf: Option<Bytes>,
}

impl VideoCache {
    pub fn new(cancel: CancellationToken) -> Arc<Self> {
        Arc::new(VideoCache {
            state: Mutex::new(CacheState {
                total: -1,
                content_type: String::new(),
                complete: false,
                err: false,
                buf: None,
            }),
            cancel,
        })
    }

    fn fail(&self) {
        self.state.lock().err = true;
    }

    fn finish(&self, buf: Bytes, total: i64, ctype: String) {
        let mut s = self.state.lock();
        s.buf = Some(buf);
        s.total = total;
        s.content_type = ctype;
        s.complete = true;
    }

    /// Returns the buffer once the whole track is cached and no error occurred.
    /// (Go `completeSnapshot`.)
    pub fn complete_snapshot(&self) -> Option<(Bytes, i64, String)> {
        let s = self.state.lock();
        if s.complete && !s.err {
            Some((s.buf.clone()?, s.total, s.content_type.clone()))
        } else {
            None
        }
    }
}

/// LRU-bounded map of in-flight / completed [`VideoCache`]s (Go `byteCache` +
/// `byteOrder`, capped at `maxCachedTracks`).
#[derive(Default)]
pub struct ByteCacheMap {
    map: std::collections::HashMap<String, Arc<VideoCache>>,
    order: Vec<String>,
}

const MAX_CACHED_TRACKS: usize = 3;

impl ByteCacheMap {
    /// Returns the cache for `video_id` if present.
    pub fn get(&self, video_id: &str) -> Option<Arc<VideoCache>> {
        self.map.get(video_id).cloned()
    }

    /// Inserts a fresh cache (created via `make`), evicting the oldest entries
    /// (cancelling their in-flight downloads) past the cap. Returns the new
    /// entry. (Go `getOrStartCache` minus the spawn, which the caller does.)
    pub fn insert(&mut self, video_id: &str, vc: Arc<VideoCache>) {
        self.map.insert(video_id.to_string(), vc);
        self.order.push(video_id.to_string());
        while self.order.len() > MAX_CACHED_TRACKS {
            let old = self.order.remove(0);
            if let Some(ev) = self.map.remove(&old) {
                ev.cancel.cancel(); // abort any in-flight download
            }
        }
    }
}

/// Caches the whole VOD audio stream into RAM via parallel ranged GETs. On
/// success the track is marked complete (the server then serves every range —
/// including seeks — from RAM); on failure the server keeps passing through to
/// the CDN for this track. (Go `downloadInto`.)
pub async fn download_into(
    client: reqwest::Client,
    direct_url: String,
    vc: Arc<VideoCache>,
    cancel: CancellationToken,
) {
    // Let playback's own (passthrough) request grab the initial buffer first.
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_millis(DL_START_DELAY_MS)) => {}
        _ = cancel.cancelled() => return,
    }

    let (total, ctype) = match probe_size(&client, &direct_url).await {
        Ok(v) => v,
        Err(_) => {
            vc.fail();
            return;
        }
    };
    if total <= 0 {
        vc.fail();
        return;
    }

    let mut segs = DL_SEGMENTS;
    if total / segs < DL_MIN_SEGMENT {
        segs = (total / DL_MIN_SEGMENT + 1).max(1);
    }
    let seg_size = total / segs;

    let mut set: JoinSet<std::result::Result<(usize, Bytes), ()>> = JoinSet::new();
    for i in 0..segs {
        let start = i * seg_size;
        let end = if i == segs - 1 { total - 1 } else { start + seg_size - 1 };
        let client = client.clone();
        let url = direct_url.clone();
        set.spawn(async move {
            match fetch_segment(&client, &url, start, end).await {
                Ok(b) => Ok((start as usize, b)),
                Err(_) => Err(()),
            }
        });
    }

    let mut buf = BytesMut::zeroed(total as usize);
    let mut failed = false;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            next = set.join_next() => {
                match next {
                    None => break,
                    Some(Ok(Ok((start, bytes)))) => {
                        let end = start + bytes.len();
                        if end <= buf.len() {
                            buf[start..end].copy_from_slice(&bytes);
                        }
                    }
                    Some(_) => { failed = true; }
                }
            }
        }
    }

    if cancel.is_cancelled() {
        return;
    }
    if failed {
        vc.fail();
        return;
    }
    vc.finish(buf.freeze(), total, ctype);
}

/// Fetches `[start, end]` via one ranged request. (Go `fetchSegment`.)
async fn fetch_segment(
    client: &reqwest::Client,
    url: &str,
    start: i64,
    end: i64,
) -> std::result::Result<Bytes, ()> {
    let resp = client
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(|_| ())?;
    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(());
    }
    resp.bytes().await.map_err(|_| ())
}

/// Fetches the total length + content type with a 1-byte ranged GET (the total
/// comes from the `Content-Range: …/<total>` suffix). (Go `probeSize`.)
async fn probe_size(
    client: &reqwest::Client,
    url: &str,
) -> std::result::Result<(i64, String), ()> {
    let resp = client
        .get(url)
        .header(reqwest::header::RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(|_| ())?;
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let total = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cr| cr.rsplit('/').next())
        .and_then(|t| t.trim().parse::<i64>().ok())
        .unwrap_or(0);
    Ok((total, ctype))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from the behaviour exercised by Go's parseRange.
    #[test]
    fn parse_range_open_ended() {
        // bytes=100- → from 100 to end.
        assert_eq!(parse_range("bytes=100-", 1000), (100, 999, true));
    }

    #[test]
    fn parse_range_closed() {
        assert_eq!(parse_range("bytes=100-199", 1000), (100, 199, true));
        // end clamped to total-1.
        assert_eq!(parse_range("bytes=100-5000", 1000), (100, 999, true));
    }

    #[test]
    fn parse_range_suffix() {
        // bytes=-100 → last 100 bytes.
        assert_eq!(parse_range("bytes=-100", 1000), (900, 999, true));
        // suffix larger than total clamps start to 0.
        assert_eq!(parse_range("bytes=-5000", 1000), (0, 999, true));
    }

    #[test]
    fn parse_range_absent_or_bad() {
        assert_eq!(parse_range("", 1000), (0, 999, false));
        assert_eq!(parse_range("items=0-1", 1000), (0, 999, false));
        assert_eq!(parse_range("bytes=abc", 1000), (0, 999, false));
    }

    #[test]
    fn parse_range_only_first() {
        // Multiple ranges: only the first honoured.
        assert_eq!(parse_range("bytes=0-99,200-299", 1000), (0, 99, true));
    }

    #[test]
    fn byte_cache_evicts_oldest() {
        let mut m = ByteCacheMap::default();
        for id in ["a", "b", "c", "d"] {
            m.insert(id, VideoCache::new(CancellationToken::new()));
        }
        assert!(m.get("a").is_none(), "oldest evicted past cap 3");
        for id in ["b", "c", "d"] {
            assert!(m.get(id).is_some());
        }
    }
}
