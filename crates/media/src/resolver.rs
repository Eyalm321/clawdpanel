//! Resolver traits — the seams between the controller/station and the
//! YouTube-extraction layer. Ports of Go `audio.StreamResolver` and the slice of
//! `radio.Resolver` the station uses (`ExpandPlaylist`). Kept as traits so the
//! controller/station unit-test against fakes and the real `rusty_ytdl` impl can
//! drift behind them (the XL extraction risk).

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::event::ResolvedTrack;

/// Resolves a YouTube video id to a playable stream URL. Mirrors Go
/// `audio.StreamResolver.Resolve(ctx, videoID, forceRefresh)`.
#[async_trait]
pub trait StreamResolver: Send + Sync {
    async fn resolve(&self, video_id: &str, force_refresh: bool) -> Result<ResolvedTrack>;
}

/// Expands a playlist id into an ordered list of video ids. Mirrors Go
/// `radio.Resolver.ExpandPlaylist`. The [`CancellationToken`] is the port of the
/// Go `context.Context` the station passes so a station switch aborts an
/// in-flight expansion (the epoch check then discards any late result anyway).
#[async_trait]
pub trait PlaylistExpander: Send + Sync {
    async fn expand_playlist(
        &self,
        playlist_id: &str,
        force_refresh: bool,
        cancel: CancellationToken,
    ) -> Result<Vec<String>>;
}
