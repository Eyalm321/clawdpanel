//! Live-DASH staticizer — a byte-for-byte port of Go's `staticizeLiveMPD*`
//! (`internal/radio/radio.go`). This is the "playing but silent" fix: it rewrites
//! YouTube's dynamic live MPD into a static one covering the freshest part of the
//! DVR window, shifting `presentationTimeOffset` + `startNumber` by the dropped
//! lead so the sink doesn't schedule audio hours into the future. Every invariant
//! is preserved verbatim, including the `timescale="1000"` assert and the
//! `r=`-repeat rejection — mis-shifting the PTO reproduces the silence bug.

use once_cell::sync::Lazy;
use regex::{Captures, Regex};

use crate::error::{Error, Result};

/// Bounds how much of the DVR window the static manifest exposes, in
/// segment-timescale units (ms — timescale is 1000 by observation). A fresh
/// ~30min tail starts on valid content and still EOSes into a re-resolved window.
const MAX_STATIC_WINDOW_MS: i64 = 30 * 60 * 1000;

static RE_LIVE_ATTRS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"\s*(yt:)?(minimumUpdatePeriod|timeShiftBufferDepth|availabilityStartTime|mpdRequestTime|mpdResponseTime|earliestMediaSequence)="[^"]*""#).unwrap()
});
static RE_SEG_DUR: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<S\b[^>]*?\bd="(\d+)""#).unwrap());
static RE_SEG_REPEAT: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<S\b[^>]*?\br=""#).unwrap());
static RE_PERIOD_START: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<Period start="PT[0-9.]+S""#).unwrap());
static RE_VIDEO_SET: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?s)<AdaptationSet[^>]*mimeType="video/[^"]*".*?</AdaptationSet>"#).unwrap());
static RE_SEG_URL: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<SegmentURL [^>]*/>"#).unwrap());
static RE_S_ENTRY: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<S\b[^>]*/>"#).unwrap());
static RE_SEG_LIST_BLOCK: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?s)<SegmentList[^>]*>.*?</SegmentList>"#).unwrap());
static RE_START_NUMBER: Lazy<Regex> = Lazy::new(|| Regex::new(r#"startNumber="\d+""#).unwrap());
static RE_PTO_ATTR: Lazy<Regex> = Lazy::new(|| Regex::new(r#"presentationTimeOffset="\d+""#).unwrap());
static RE_INT: Lazy<Regex> = Lazy::new(|| Regex::new(r#"\d+"#).unwrap());

/// Static-manifest window bound, overridable via `CLAWDPANEL_LIVE_WINDOW_MS`
/// (for testing the EOS→advance loop without waiting out the full window). The
/// override is logged once. (Go `liveWindowMs`.)
fn live_window_ms() -> i64 {
    use std::sync::Once;
    static LOG_ONCE: Once = Once::new();
    if let Ok(v) = std::env::var("CLAWDPANEL_LIVE_WINDOW_MS") {
        if let Ok(n) = v.parse::<i64>() {
            if n > 0 {
                LOG_ONCE.call_once(|| {
                    log::info!("[radio] live manifest window overridden to {n}ms (CLAWDPANEL_LIVE_WINDOW_MS)");
                });
                return n;
            }
        }
    }
    MAX_STATIC_WINDOW_MS
}

/// Rewrites YouTube's dynamic live MPD into a static one covering the freshest
/// part of the DVR window. (Go `staticizeLiveMPD`.)
pub fn staticize_live_mpd(body: &str) -> Result<String> {
    staticize_live_mpd_window(body, live_window_ms())
}

/// [`staticize_live_mpd`] with an explicit window bound (the tests exercise the
/// trim with a small window). (Go `staticizeLiveMPDWindow`.)
pub fn staticize_live_mpd_window(body: &str, window_ms: i64) -> Result<String> {
    if !body.contains(r#"type="dynamic""#) {
        return Err(Error::new("MPD is not dynamic"));
    }
    // Drop the video AdaptationSets first (~90% of the doc); the remaining
    // passes then scan a few KB instead.
    let mut out = RE_VIDEO_SET.replace_all(body, "").into_owned();
    out = out.replacen(r#"type="dynamic""#, r#"type="static""#, 1);
    out = RE_LIVE_ATTRS.replace_all(&out, "").into_owned();
    out = RE_PERIOD_START.replace_all(&out, "<Period").into_owned();

    // The PTO arithmetic below assumes ms-granularity <S d="..."> entries, one
    // per segment. Fail loudly if YouTube switches timescale or to r= compaction
    // — silently mis-shifting the PTO reproduces the scheduled-hours-ahead
    // silence this rewrite exists to prevent.
    if !out.contains(r#"timescale="1000""#) {
        return Err(Error::new("segment timeline is not timescale=1000"));
    }
    if RE_SEG_REPEAT.is_match(&out) {
        return Err(Error::new("segment timeline uses r= repeat compaction"));
    }

    let s_entries: Vec<&str> = RE_S_ENTRY.find_iter(&out).map(|m| m.as_str()).collect();
    if s_entries.is_empty() {
        return Err(Error::new("MPD has no segment timeline"));
    }
    let seg_ms = RE_SEG_DUR
        .captures(&out)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<i64>().ok())
        .unwrap_or(0);
    if seg_ms <= 0 {
        return Err(Error::new("MPD has no segment duration"));
    }
    let mut keep = (window_ms / seg_ms) as usize;
    if keep < 1 {
        keep = 1;
    }
    if keep > s_entries.len() {
        keep = s_entries.len();
    }

    let drop_count = s_entries.len() - keep;
    let mut dropped_ms: i64 = 0;
    for (i, s) in s_entries.iter().enumerate() {
        let Some(c) = RE_SEG_DUR.captures(s) else {
            continue;
        };
        let ms = c
            .get(1)
            .unwrap()
            .as_str()
            .parse::<i64>()
            .map_err(|e| Error::new(format!("parse segment duration: {e}")))?;
        if i < drop_count {
            dropped_ms += ms;
        }
    }
    // keptMs is the sum of the kept tail — recomputed here so the float format
    // below matches Go's (sum, not count×nominal).
    let kept_ms: i64 = s_entries
        .iter()
        .skip(drop_count)
        .filter_map(|s| RE_SEG_DUR.captures(s))
        .filter_map(|c| c.get(1).and_then(|m| m.as_str().parse::<i64>().ok()))
        .sum();

    out = RE_SEG_LIST_BLOCK
        .replace_all(&out, |caps: &Captures| {
            let mut block = caps[0].to_string();
            block = trim_leading(&block, &RE_S_ENTRY, keep);
            block = trim_leading(&block, &RE_SEG_URL, keep);
            // Shift the attributed SegmentList tag (the one carrying
            // startNumber/PTO) to the trimmed window's start; bare per-
            // Representation tags have neither attribute and pass unchanged.
            block = replace_int_attr(&block, &RE_START_NUMBER, "startNumber", |v| v + drop_count as i64);
            replace_int_attr(&block, &RE_PTO_ATTR, "presentationTimeOffset", |v| v + dropped_ms)
        })
        .into_owned();

    out = out.replacen(
        "<MPD ",
        &format!(
            r#"<MPD mediaPresentationDuration="PT{:.3}S" "#,
            kept_ms as f64 / 1000.0
        ),
        1,
    );
    Ok(out)
}

/// Rewrites the integer attribute matched by `re` inside `tag` by applying `f`.
/// Tags without the attribute pass unchanged. (Go `replaceInt64Attr`.)
fn replace_int_attr(tag: &str, re: &Regex, name: &str, f: impl Fn(i64) -> i64) -> String {
    re.replace_all(tag, |caps: &Captures| {
        let attr = &caps[0];
        match RE_INT.find(attr).and_then(|m| m.as_str().parse::<i64>().ok()) {
            Some(v) => format!(r#"{name}="{}""#, f(v)),
            None => attr.to_string(),
        }
    })
    .into_owned()
}

/// Removes all but the last `keep` matches of `re` from `body` (keeping the
/// freshest tail + the text between matches). (Go `trimLeading`.)
fn trim_leading(body: &str, re: &Regex, keep: usize) -> String {
    let locs: Vec<(usize, usize)> = re.find_iter(body).map(|m| (m.start(), m.end())).collect();
    if locs.len() <= keep {
        return body.to_string();
    }
    let drop = &locs[..locs.len() - keep];
    let mut out = String::with_capacity(body.len());
    let mut prev = 0;
    for &(start, end) in drop {
        out.push_str(&body[prev..start]);
        prev = end;
    }
    out.push_str(&body[prev..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixture facts (from the captured manifest; see Go staticize_test.go).
    const FIXTURE_PTO: i64 = 7518965733;
    const FIXTURE_START: i64 = 1503782;
    const FIXTURE_SEG_MS: i64 = 5000;
    const FIXTURE_SEGS: i64 = 12;
    const FIXTURE_LAST_SEG_URL: &str = r#"media="sq/1506661/"#;

    fn fixture() -> String {
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/live-dynamic.mpd"))
            .expect("read fixture")
    }

    fn well_formed(s: &str) -> bool {
        // A crude well-formedness check: balanced angle brackets and no stray
        // dynamic attrs. (The Go test uses encoding/xml; we keep it dependency
        // -free but still catch a mangled trim.)
        let opens = s.matches('<').count();
        let closes = s.matches('>').count();
        opens == closes && opens > 0
    }

    #[test]
    fn staticize_untrimmed_keeps_upstream_offsets() {
        let out = staticize_live_mpd(&fixture()).expect("staticize");
        assert!(well_formed(&out));
        assert!(out.contains(r#"type="static""#) && !out.contains(r#"type="dynamic""#));
        for attr in ["minimumUpdatePeriod", "timeShiftBufferDepth", "availabilityStartTime"] {
            assert!(!out.contains(attr), "dynamic attribute {attr} survived");
        }
        assert!(!out.contains(r#"mimeType="video/"#), "video AdaptationSet survived");

        // 30min window ≫ the fixture's 12×5s: nothing trimmed, offsets unchanged.
        assert!(out.contains(&format!(r#"presentationTimeOffset="{FIXTURE_PTO}""#)));
        assert!(out.contains(&format!(r#"startNumber="{FIXTURE_START}""#)));
        assert!(out.contains(r#"mediaPresentationDuration="PT60.000S""#));
    }

    #[test]
    fn staticize_trim_shifts_pto_and_start() {
        // 20s window over 5s segments: keep the last 4, drop the leading 8.
        let out = staticize_live_mpd_window(&fixture(), 20 * 1000).expect("staticize");
        assert!(well_formed(&out));

        let dropped = FIXTURE_SEGS - 4;
        let want_pto = FIXTURE_PTO + dropped * FIXTURE_SEG_MS;
        let want_start = FIXTURE_START + dropped;
        assert!(out.contains(&format!(r#"presentationTimeOffset="{want_pto}""#)), "PTO not shifted");
        assert!(out.contains(&format!(r#"startNumber="{want_start}""#)), "startNumber not shifted");
        assert!(out.contains(r#"mediaPresentationDuration="PT20.000S""#));

        // Every per-Representation URL list trimmed to 4 (two audio Reps → 8).
        assert_eq!(RE_SEG_URL.find_iter(&out).count(), 8);
        assert!(out.contains(FIXTURE_LAST_SEG_URL), "freshest segment lost");
        assert_eq!(RE_S_ENTRY.find_iter(&out).count(), 4, "want 4 timeline <S> entries");
    }

    #[test]
    fn rejects_non_dynamic() {
        assert!(staticize_live_mpd(r#"<MPD type="static"></MPD>"#).is_err());
    }
}
