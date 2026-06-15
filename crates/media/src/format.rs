//! Audio-format selection — the portable core of Go's `radio.pickAudioFormat`,
//! expressed over a small [`FormatLike`] trait so it is unit-testable without
//! pulling in the YouTube-extraction crate. The `rusty_ytdl` resolver implements
//! [`FormatLike`] for its own format type and calls [`pick_audio_index`].

/// The subset of a YouTube format the picker reasons about.
pub trait FormatLike {
    /// The `mimeType` string, e.g. `audio/mp4; codecs="mp4a.40.2"`.
    fn mime_type(&self) -> &str;
    /// Whether the format carries audio channels (`audioChannels > 0`).
    fn has_audio(&self) -> bool;
    /// Sort key: average bitrate (bits/s). Higher is preferred, matching
    /// kkdai/youtube's `FormatList.Sort` for audio-only formats (which have no
    /// width to sort on, so it falls through to bitrate descending).
    fn bitrate(&self) -> u64;
}

/// Selects the index of the best audio-only format: `audio/mp4` (itag 140 AAC)
/// first as it plays in all native backends; then any format that carries audio
/// channels; then any format at all as a last resort. Within a tier the highest
/// bitrate wins (the `Sort` + `[0]` in the Go original). Returns `None` only for
/// an empty list.
pub fn pick_audio_index<F: FormatLike>(formats: &[F]) -> Option<usize> {
    // audio/mp4 with audio channels — highest bitrate.
    let mp4 = best_by_bitrate(formats, |f| f.has_audio() && f.mime_type().starts_with("audio/mp4"));
    if mp4.is_some() {
        return mp4;
    }
    // any format with audio channels — highest bitrate.
    let any_audio = best_by_bitrate(formats, |f| f.has_audio());
    if any_audio.is_some() {
        return any_audio;
    }
    // last resort: the first format at all.
    if formats.is_empty() {
        None
    } else {
        Some(0)
    }
}

/// Index of the highest-bitrate format matching `pred`, preferring the earlier
/// entry on ties (a stable descending sort would keep the first).
fn best_by_bitrate<F: FormatLike>(formats: &[F], pred: impl Fn(&F) -> bool) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, f) in formats.iter().enumerate() {
        if !pred(f) {
            continue;
        }
        match best {
            Some(b) if formats[b].bitrate() >= f.bitrate() => {}
            _ => best = Some(i),
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fmt {
        mime: &'static str,
        audio: bool,
        br: u64,
    }
    impl FormatLike for Fmt {
        fn mime_type(&self) -> &str {
            self.mime
        }
        fn has_audio(&self) -> bool {
            self.audio
        }
        fn bitrate(&self) -> u64 {
            self.br
        }
    }

    #[test]
    fn prefers_audio_mp4_highest_bitrate() {
        // itag 139 (48k m4a), itag 140 (128k m4a), a webm/opus, and a muxed video.
        let f = [
            Fmt { mime: "video/mp4; codecs=\"avc1\"", audio: true, br: 1_000_000 },
            Fmt { mime: "audio/mp4; codecs=\"mp4a.40.2\"", audio: true, br: 49_000 },
            Fmt { mime: "audio/webm; codecs=\"opus\"", audio: true, br: 160_000 },
            Fmt { mime: "audio/mp4; codecs=\"mp4a.40.2\"", audio: true, br: 128_000 },
        ];
        // audio/mp4 wins over the higher-bitrate opus; among mp4, 128k > 48k.
        assert_eq!(pick_audio_index(&f), Some(3));
    }

    #[test]
    fn falls_back_to_any_audio_then_anything() {
        let only_opus = [Fmt { mime: "audio/webm; codecs=\"opus\"", audio: true, br: 160_000 }];
        assert_eq!(pick_audio_index(&only_opus), Some(0));

        let no_audio = [Fmt { mime: "video/mp4", audio: false, br: 500_000 }];
        assert_eq!(pick_audio_index(&no_audio), Some(0));

        let empty: [Fmt; 0] = [];
        assert_eq!(pick_audio_index(&empty), None);
    }
}
