//! Station-source parsing — a 1:1 port of Go's `internal/station/parse.go`:
//! turn a user-pasted YouTube URL or bare ID into a typed [`StationItem`], and
//! report whether a station can ever step through more than one track. The
//! settings editor uses [`parse_item`] for server-side validation (the old
//! `ParseStationItem` binding); the bar uses [`has_multiple_tracks`] to gate its
//! track-skip buttons.

use once_cell::sync::Lazy;
use regex::Regex;

use clawdpanel_types::{StationConfig, StationItem, StationItemKind};

// videoRefRe: the 11-char video ID from any common YouTube URL form, regardless
// of scheme/host. Checked before list= so watch?v=X&list=Y resolves to playlist Y
// (handled by the list= check running first) while youtu.be/X?list=Y stays a video.
static VIDEO_REF_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:youtu\.be/|/shorts/|/embed/|/v/|/live/|[?&]v=)([0-9A-Za-z_-]{11})").unwrap()
});

// listRe: a playlist ID from a list= parameter (13+ chars of the URL-safe alphabet).
static LIST_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[?&]list=([0-9A-Za-z_-]{13,})").unwrap());

// Bare IDs pasted without any URL. Video IDs are exactly 11 chars; playlist IDs
// are 13+ — the length gap disambiguates them.
static BARE_VIDEO_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[0-9A-Za-z_-]{11}$").unwrap());
static BARE_PLAYLIST_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[0-9A-Za-z_-]{13,}$").unwrap());

/// Classifies a single user input into a [`StationItem`]. Accepts every common
/// form — with or without scheme/host, and bare IDs:
///
/// * `watch?v=X&list=Y` → playlist `Y` (playlist wins)
/// * `watch?v=X` → video `X`
/// * `youtu.be/X`, `/shorts/X`, `/embed/X`, `/v/X`, `/live/X` → video `X`
/// * `playlist?list=Y`, `watch?list=Y`, `…&list=Y` → playlist `Y`
/// * bare 11-char ID → video
/// * bare 13+-char ID → playlist
///
/// Live-vs-VOD is deferred to the resolver, so a watch/live URL is always
/// `Video` here. Returns the original (trimmed) input as `raw`, for the editor.
pub fn parse_item(input: &str) -> Result<StationItem, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("empty input".to_string());
    }

    if let Some(c) = LIST_RE.captures(raw) {
        return Ok(StationItem {
            kind: StationItemKind::Playlist,
            id: c[1].to_string(),
            raw: raw.to_string(),
        });
    }
    if let Some(c) = VIDEO_REF_RE.captures(raw) {
        return Ok(StationItem {
            kind: StationItemKind::Video,
            id: c[1].to_string(),
            raw: raw.to_string(),
        });
    }
    if BARE_VIDEO_RE.is_match(raw) {
        return Ok(StationItem {
            kind: StationItemKind::Video,
            id: raw.to_string(),
            raw: raw.to_string(),
        });
    }
    if BARE_PLAYLIST_RE.is_match(raw) {
        return Ok(StationItem {
            kind: StationItemKind::Playlist,
            id: raw.to_string(),
            raw: raw.to_string(),
        });
    }
    Err(format!("unrecognized YouTube URL or ID: {raw:?}"))
}

/// Reports whether a station can ever have more than one track to step through —
/// i.e. it has two or more items, or its single item is (or re-parses to) a
/// playlist. Re-parses each item's `raw` exactly as the player would, so a
/// `watch?v=…&list=…` saved with a stale `Video` kind is still recognised as a
/// playlist. Never touches the network.
pub fn has_multiple_tracks(st: &StationConfig) -> bool {
    if st.items.len() >= 2 {
        return true;
    }
    if st.items.len() != 1 {
        return false;
    }
    let mut it = st.items[0].clone();
    if !it.raw.is_empty() {
        if let Ok(parsed) = parse_item(&it.raw) {
            it = parsed;
        }
    }
    it.kind == StationItemKind::Playlist
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from Go `internal/station/parse_test.go::TestParseItem`.
    #[test]
    fn parse_item_cases() {
        struct Case {
            input: &'static str,
            kind: StationItemKind,
            id: &'static str,
        }
        let ok = |input, kind, id| Case { input, kind, id };
        let cases = [
            ok("YmQ7jRgf4f0", StationItemKind::Video, "YmQ7jRgf4f0"),
            ok("PLAbcdEfGhIjKlMnOpQrSt", StationItemKind::Playlist, "PLAbcdEfGhIjKlMnOpQrSt"),
            ok("https://www.youtube.com/watch?v=EWrX250Zhko", StationItemKind::Video, "EWrX250Zhko"),
            ok("https://youtube.com/watch?v=EWrX250Zhko", StationItemKind::Video, "EWrX250Zhko"),
            ok("www.youtube.com/watch?v=EWrX250Zhko", StationItemKind::Video, "EWrX250Zhko"),
            ok("youtube.com/watch?v=EWrX250Zhko", StationItemKind::Video, "EWrX250Zhko"),
            ok("https://music.youtube.com/watch?v=EWrX250Zhko", StationItemKind::Video, "EWrX250Zhko"),
            ok("https://youtu.be/EWrX250Zhko", StationItemKind::Video, "EWrX250Zhko"),
            ok("https://youtu.be/EWrX250Zhko?si=AbCdEfGhIj", StationItemKind::Video, "EWrX250Zhko"),
            ok("https://www.youtube.com/shorts/EWrX250Zhko", StationItemKind::Video, "EWrX250Zhko"),
            ok("https://www.youtube.com/embed/EWrX250Zhko", StationItemKind::Video, "EWrX250Zhko"),
            ok("https://www.youtube.com/v/EWrX250Zhko", StationItemKind::Video, "EWrX250Zhko"),
            ok("https://www.youtube.com/live/EWrX250Zhko", StationItemKind::Video, "EWrX250Zhko"),
            ok(
                "https://www.youtube.com/watch?v=EWrX250Zhko&list=PLAbcdEfGhIjKlMnOpQrSt",
                StationItemKind::Playlist,
                "PLAbcdEfGhIjKlMnOpQrSt",
            ),
            ok(
                "https://youtu.be/EWrX250Zhko?list=PLAbcdEfGhIjKlMnOpQrSt",
                StationItemKind::Playlist,
                "PLAbcdEfGhIjKlMnOpQrSt",
            ),
            ok(
                "https://www.youtube.com/playlist?list=PLAbcdEfGhIjKlMnOpQrSt",
                StationItemKind::Playlist,
                "PLAbcdEfGhIjKlMnOpQrSt",
            ),
            ok(
                "www.youtube.com/playlist?list=PLAbcdEfGhIjKlMnOpQrSt",
                StationItemKind::Playlist,
                "PLAbcdEfGhIjKlMnOpQrSt",
            ),
            ok(
                "https://www.youtube.com/playlist?list=PLAbcdEfGhIjKlMnOpQrSt&si=xyz",
                StationItemKind::Playlist,
                "PLAbcdEfGhIjKlMnOpQrSt",
            ),
            ok(
                "https://www.youtube.com/watch?list=PLAbcdEfGhIjKlMnOpQrSt",
                StationItemKind::Playlist,
                "PLAbcdEfGhIjKlMnOpQrSt",
            ),
            ok("  YmQ7jRgf4f0  ", StationItemKind::Video, "YmQ7jRgf4f0"),
        ];
        for c in cases {
            let got = parse_item(c.input).unwrap_or_else(|e| panic!("parse_item({:?}) errored: {e}", c.input));
            assert_eq!(got.kind, c.kind, "kind for {:?}", c.input);
            assert_eq!(got.id, c.id, "id for {:?}", c.input);
        }
    }

    #[test]
    fn parse_item_errors() {
        for bad in ["", "hello", "abcdefghijkl" /* 12 chars: neither video nor playlist */] {
            assert!(parse_item(bad).is_err(), "expected error for {bad:?}");
        }
    }

    // Ported from Go `internal/station/parse_test.go::TestHasMultipleTracks`.
    #[test]
    fn has_multiple_tracks_cases() {
        let v = |id: &str| StationItem { kind: StationItemKind::Video, id: id.into(), raw: String::new() };
        let live = |id: &str| StationItem { kind: StationItemKind::Livestream, id: id.into(), raw: String::new() };
        let pl = |id: &str| StationItem { kind: StationItemKind::Playlist, id: id.into(), raw: String::new() };
        let st = |items: Vec<StationItem>| StationConfig { name: "S".into(), items, shuffle: false };

        assert!(!has_multiple_tracks(&st(vec![])));
        assert!(!has_multiple_tracks(&st(vec![v("YmQ7jRgf4f0")])));
        assert!(!has_multiple_tracks(&st(vec![live("EWrX250Zhko")])));
        assert!(has_multiple_tracks(&st(vec![pl("PLAbcdEfGhIjKlMnOpQrSt")])));
        assert!(has_multiple_tracks(&st(vec![v("YmQ7jRgf4f0"), v("EWrX250Zhko")])));

        // Stale "video" kind but the raw URL carries a list= → expands to many tracks.
        let stale = StationItem {
            kind: StationItemKind::Video,
            id: "6TnV43UWoqk".into(),
            raw: "https://www.youtube.com/watch?v=6TnV43UWoqk&list=PLLvWV__Bn2_PwR92FfrxjsZCAM7zyxzze".into(),
        };
        assert!(has_multiple_tracks(&st(vec![stale])));
    }
}
