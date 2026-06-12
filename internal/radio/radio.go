// Package radio resolves YouTube videos to playable stream URLs.
//
// Livestreams resolve to an HLS manifest URL (played via a top-level <audio>
// element / native player instead of the YouTube IFrame embed — the latter
// silently stays muted in macOS WKWebView's cross-origin iframe even with the
// autoplay grant). Regular VOD videos resolve to a deciphered, audio-only
// direct googlevideo URL. Playlists expand to an ordered list of video IDs.
package radio

import (
	"context"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/kkdai/youtube/v2"
)

// YouTube signs stream URLs with ~6h expiry; refresh well before. Playlist
// membership changes far less often, so it gets a much longer TTL.
const (
	cacheTTL         = 1 * time.Hour
	playlistCacheTTL = 6 * time.Hour
)

// maxCachedTracks bounds how many fully-buffered VOD audio streams we keep in
// memory at once (audio-only, ~tens of MB each). The station plays one track at
// a time, so a small cap covers the current track plus a couple of neighbours.
const maxCachedTracks = 3

// ResolvedTrack is a playable stream URL plus whether it is a livestream.
// Livestreams (IsLive) never end on their own — the station player must not
// expect a StateEnded for them.
type ResolvedTrack struct {
	URL    string
	IsLive bool
}

type trackEntry struct {
	track ResolvedTrack
	at    time.Time
}

type playlistEntry struct {
	ids []string
	at  time.Time
}

type Resolver struct {
	client        youtube.Client
	mu            sync.Mutex
	cache         map[string]trackEntry
	playlistCache map[string]playlistEntry
	port          int

	// byteCache holds progressive in-memory copies of recently played VOD audio
	// streams, so MediaPlayer's range requests (initial play, its constant
	// re-buffering, and especially seeks) are served from RAM instead of
	// re-fetching from googlevideo on every chunk. Guarded by byteCacheMu;
	// byteOrder tracks insertion order for the maxCachedTracks cap.
	byteCacheMu sync.Mutex
	byteCache   map[string]*videoCache
	byteOrder   []string
}

func New() *Resolver {
	r := &Resolver{
		cache:         map[string]trackEntry{},
		playlistCache: map[string]playlistEntry{},
		byteCache:     map[string]*videoCache{},
	}

	// Start local proxy server to stream YouTube VODs safely (bypasses 403 Forbidden).
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err == nil {
		r.port = listener.Addr().(*net.TCPAddr).Port
		go func() {
			_ = http.Serve(listener, r)
		}()
	}

	return r
}

// ServeHTTP answers the native player's range requests for a VOD's audio.
//
// Once the whole track has been buffered into RAM by the background download
// (downloadInto), every request — initial play, MediaPlayer's chunked
// re-buffering, and especially seeks — is served from that buffer: instant, no
// CDN round-trip. Until the buffer is complete we pass the request straight
// through to googlevideo (the original behaviour). We deliberately do NOT block
// on not-yet-downloaded bytes: MediaPlayer probes arbitrary offsets (e.g. the
// MP4 index near the end of the file), and forcing it to wait for our sequential
// download to reach them is what made playback hang.
func (r *Resolver) ServeHTTP(w http.ResponseWriter, req *http.Request) {
	videoID := req.URL.Query().Get("id")
	if videoID == "" {
		http.Error(w, "missing video id", http.StatusBadRequest)
		return
	}
	vc := r.getOrStartCache(videoID)

	if buf, total, ctype, ok := vc.completeSnapshot(); ok {
		serveFromBuffer(w, req, buf, total, ctype)
		return
	}
	r.passthrough(w, req, videoID)
}

// serveFromBuffer answers a (possibly ranged) request from a fully-buffered,
// immutable copy of the track held in RAM. No blocking — every byte is present.
func serveFromBuffer(w http.ResponseWriter, req *http.Request, buf []byte, total int64, ctype string) {
	start, end, isRange := parseRange(req.Header.Get("Range"), total)
	w.Header().Set("Accept-Ranges", "bytes")
	if ctype != "" {
		w.Header().Set("Content-Type", ctype)
	}
	if isRange {
		w.Header().Set("Content-Range", fmt.Sprintf("bytes %d-%d/%d", start, end, total))
		w.Header().Set("Content-Length", strconv.FormatInt(end-start+1, 10))
		w.WriteHeader(http.StatusPartialContent)
	} else {
		w.Header().Set("Content-Length", strconv.FormatInt(total, 10))
		w.WriteHeader(http.StatusOK)
	}
	if req.Method == http.MethodHead {
		return
	}
	if start < 0 {
		start = 0
	}
	if hi := int64(len(buf)) - 1; end > hi {
		end = hi
	}
	if start <= end {
		_, _ = w.Write(buf[start : end+1]) // MediaPlayer cancels once satisfied
	}
}

// passthrough proxies a single range request straight to googlevideo (the
// pre-cache behaviour), used while the in-RAM copy is still downloading.
func (r *Resolver) passthrough(w http.ResponseWriter, req *http.Request, videoID string) {
	track, err := r.resolveDirect(req.Context(), videoID, false)
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	proxyReq, err := http.NewRequestWithContext(req.Context(), http.MethodGet, track.URL, nil)
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	if rng := req.Header.Get("Range"); rng != "" {
		proxyReq.Header.Set("Range", rng)
	}
	resp, err := http.DefaultClient.Do(proxyReq)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadGateway)
		return
	}
	defer resp.Body.Close()
	for _, h := range []string{"Content-Type", "Content-Length", "Content-Range"} {
		if v := resp.Header.Get(h); v != "" {
			w.Header().Set(h, v)
		}
	}
	w.Header().Set("Accept-Ranges", "bytes")
	w.WriteHeader(resp.StatusCode)
	_, _ = io.Copy(w, resp.Body)
}

// videoCache is an in-memory copy of one VOD's audio stream, filled once by a
// background download (downloadInto). It is served to the player only after the
// download completes (completeSnapshot); the buffer is immutable thereafter, so
// it can be shared with readers without copying.
type videoCache struct {
	mu          sync.Mutex
	buf         []byte
	total       int64
	contentType string
	complete    bool
	err         error
	cancel      context.CancelFunc
}

func newVideoCache(cancel context.CancelFunc) *videoCache {
	return &videoCache{total: -1, cancel: cancel}
}

func (vc *videoCache) setHeaders(total int64, ctype string) {
	vc.mu.Lock()
	vc.total, vc.contentType = total, ctype
	if total > 0 {
		// Full-length allocation up front: the parallel segment downloads each
		// write a disjoint slice of this buffer, so it's never reallocated.
		vc.buf = make([]byte, total)
	}
	vc.mu.Unlock()
}

func (vc *videoCache) finish() {
	vc.mu.Lock()
	vc.complete = true
	if vc.total < 0 {
		vc.total = int64(len(vc.buf))
	}
	vc.mu.Unlock()
}

func (vc *videoCache) fail(err error) {
	vc.mu.Lock()
	if vc.err == nil {
		vc.err = err
	}
	vc.mu.Unlock()
}

// completeSnapshot returns the buffer once the whole track is cached (and no
// error occurred). The returned slice is immutable and safe to serve directly.
func (vc *videoCache) completeSnapshot() (buf []byte, total int64, ctype string, ok bool) {
	vc.mu.Lock()
	defer vc.mu.Unlock()
	if vc.complete && vc.err == nil {
		return vc.buf, vc.total, vc.contentType, true
	}
	return nil, 0, "", false
}

// getOrStartCache returns the cache for videoID, kicking off its one-time
// background download on first request and evicting the oldest entry past the cap.
func (r *Resolver) getOrStartCache(videoID string) *videoCache {
	r.byteCacheMu.Lock()
	defer r.byteCacheMu.Unlock()
	if vc, ok := r.byteCache[videoID]; ok {
		return vc
	}
	ctx, cancel := context.WithCancel(context.Background())
	vc := newVideoCache(cancel)
	r.byteCache[videoID] = vc
	r.byteOrder = append(r.byteOrder, videoID)
	for len(r.byteOrder) > maxCachedTracks {
		old := r.byteOrder[0]
		r.byteOrder = r.byteOrder[1:]
		if ev, ok := r.byteCache[old]; ok {
			ev.cancel() // abort any in-flight download; memory frees once readers drain
			delete(r.byteCache, old)
		}
	}
	go r.downloadInto(ctx, videoID, vc)
	return vc
}

// Download tuning. YouTube throttles a single connection (typically to ~playback
// rate after an initial burst), so a long track on one stream takes minutes to
// cache. Fetching disjoint segments concurrently multiplies throughput and lets
// even a 70-minute mix finish caching in seconds — after which seeks are instant.
const (
	dlSegments   = 8
	dlMinSegment = 1 << 20                // 1 MiB: don't over-split small files
	dlStartDelay = 1500 * time.Millisecond // head start for playback before we fan out
)

// downloadInto caches the whole VOD audio stream into vc.buf using parallel
// ranged requests. On success the track is marked complete (ServeHTTP then serves
// every range — including seeks — from RAM); on failure ServeHTTP keeps passing
// through to the CDN for this track.
func (r *Resolver) downloadInto(ctx context.Context, videoID string, vc *videoCache) {
	track, err := r.resolveDirect(ctx, videoID, false)
	if err != nil {
		vc.fail(fmt.Errorf("radio: resolve %s: %w", videoID, err))
		return
	}

	// Let playback's own (passthrough) request grab the initial buffer first, so
	// the parallel fan-out below doesn't starve the start of the track.
	select {
	case <-time.After(dlStartDelay):
	case <-ctx.Done():
		return
	}

	total, ctype, err := probeSize(ctx, track.URL)
	if err != nil {
		vc.fail(err)
		return
	}
	if total <= 0 {
		vc.fail(fmt.Errorf("radio: cache %s: unknown content length", videoID))
		return
	}
	vc.setHeaders(total, ctype)
	buf := vc.buf // header set above; never reallocated, safe to share with segments
	log.Printf("[Proxy] caching %s (%d bytes, %d segments)", videoID, total, dlSegments)

	segs := int64(dlSegments)
	if total/segs < dlMinSegment {
		if segs = total/dlMinSegment + 1; segs < 1 {
			segs = 1
		}
	}
	segSize := total / segs

	var wg sync.WaitGroup
	for i := int64(0); i < segs; i++ {
		start := i * segSize
		end := start + segSize - 1
		if i == segs-1 {
			end = total - 1
		}
		wg.Add(1)
		go func(start, end int64) {
			defer wg.Done()
			if e := fetchSegment(ctx, track.URL, start, end, buf); e != nil {
				vc.fail(e)
			}
		}(start, end)
	}
	wg.Wait()

	if ctx.Err() != nil {
		return // evicted/cancelled
	}
	vc.mu.Lock()
	failed := vc.err != nil
	vc.mu.Unlock()
	if failed {
		log.Printf("[Proxy] caching %s failed; staying on passthrough", videoID)
		return
	}
	vc.finish()
	log.Printf("[Proxy] cached %s complete", videoID)
}

// fetchSegment downloads buf[start:end+1] via one ranged request.
func fetchSegment(ctx context.Context, url string, start, end int64, buf []byte) error {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return err
	}
	req.Header.Set("Range", fmt.Sprintf("bytes=%d-%d", start, end))
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusPartialContent {
		return fmt.Errorf("radio: segment %d-%d: status %d", start, end, resp.StatusCode)
	}
	_, err = io.ReadFull(resp.Body, buf[start:end+1])
	return err
}

// probeSize fetches the total length and content type with a 1-byte ranged GET
// (the total comes from the Content-Range header's "/<total>" suffix).
func probeSize(ctx context.Context, url string) (total int64, ctype string, err error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return 0, "", err
	}
	req.Header.Set("Range", "bytes=0-0")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return 0, "", err
	}
	defer resp.Body.Close()
	_, _ = io.Copy(io.Discard, resp.Body)
	ctype = resp.Header.Get("Content-Type")
	if cr := resp.Header.Get("Content-Range"); cr != "" {
		if i := strings.LastIndexByte(cr, '/'); i >= 0 {
			total, _ = strconv.ParseInt(strings.TrimSpace(cr[i+1:]), 10, 64)
		}
	}
	return total, ctype, nil
}

// parseRange parses a single "bytes=start-end" header against the known total.
// Only the first range is honoured (the native players send a single range).
// Returns isRange=false for an absent/unparseable header (caller serves a 200).
func parseRange(h string, total int64) (start, end int64, isRange bool) {
	end = total - 1
	if !strings.HasPrefix(h, "bytes=") {
		return 0, end, false
	}
	spec := strings.TrimPrefix(h, "bytes=")
	if i := strings.IndexByte(spec, ','); i >= 0 {
		spec = spec[:i]
	}
	dash := strings.IndexByte(spec, '-')
	if dash < 0 {
		return 0, end, false
	}
	startStr, endStr := spec[:dash], spec[dash+1:]
	if startStr == "" { // suffix range: bytes=-N (last N bytes)
		if n, err := strconv.ParseInt(endStr, 10, 64); err == nil && n > 0 {
			if start = total - n; start < 0 {
				start = 0
			}
			return start, total - 1, true
		}
		return 0, end, false
	}
	s, err := strconv.ParseInt(startStr, 10, 64)
	if err != nil {
		return 0, end, false
	}
	start = s
	if endStr != "" {
		if e, perr := strconv.ParseInt(endStr, 10, 64); perr == nil {
			end = e
		}
	}
	if end >= total {
		end = total - 1
	}
	if start > end {
		start = end
	}
	return start, end, true
}

// resolveDirect resolves the actual underlying stream URL (either HLS manifest or direct googlevideo URL).
func (r *Resolver) resolveDirect(ctx context.Context, videoID string, forceRefresh bool) (ResolvedTrack, error) {
	videoID = strings.TrimSpace(videoID)
	if videoID == "" {
		return ResolvedTrack{}, fmt.Errorf("radio: empty video id")
	}
	r.mu.Lock()
	if !forceRefresh {
		if entry, ok := r.cache[videoID]; ok && entry.track.URL != "" && time.Since(entry.at) < cacheTTL {
			r.mu.Unlock()
			return entry.track, nil
		}
	}
	r.mu.Unlock()

	video, err := r.client.GetVideoContext(ctx, videoID)
	if err != nil {
		return ResolvedTrack{}, fmt.Errorf("youtube: get video info for %s: %w", videoID, err)
	}

	// Livestream: prefer the DASH manifest — its audio is a separate fMP4
	// track. The HLS variants are video+audio muxed into MPEG-TS whose
	// segments carry continuity-counter discontinuities at every boundary,
	// audible as a click every segment (~2s) on quiet material. DASH is
	// what YouTube's own player uses. HLS stays as the fallback.
	if video.DASHManifestURL != "" || video.HLSManifestURL != "" {
		url := video.DASHManifestURL
		if url == "" {
			url = video.HLSManifestURL
		}
		track := ResolvedTrack{URL: url, IsLive: true}
		r.mu.Lock()
		r.cache[videoID] = trackEntry{track: track, at: time.Now()}
		r.mu.Unlock()
		return track, nil
	}

	// VOD: pick an audio-only format, preferring audio/mp4 (itag 140 AAC).
	format := pickAudioFormat(video.Formats)
	if format == nil {
		return ResolvedTrack{}, fmt.Errorf("youtube: no playable format for %s", videoID)
	}
	url, err := r.client.GetStreamURLContext(ctx, video, format)
	if err != nil {
		return ResolvedTrack{}, fmt.Errorf("youtube: stream url for %s: %w", videoID, err)
	}
	track := ResolvedTrack{URL: url, IsLive: false}
	r.mu.Lock()
	r.cache[videoID] = trackEntry{track: track, at: time.Now()}
	r.mu.Unlock()
	return track, nil
}

// Resolve returns the player-facing URL. For livestreams, this is the direct HLS manifest;
// for VODs, this is our local proxy URL that forwards requests to avoid 403 Forbidden.
func (r *Resolver) Resolve(ctx context.Context, videoID string, forceRefresh bool) (ResolvedTrack, error) {
	// First resolve the direct track info so we know if it's a livestream
	track, err := r.resolveDirect(ctx, videoID, forceRefresh)
	if err != nil {
		return ResolvedTrack{}, err
	}

	// If it's a livestream or proxy port is not set, play it directly
	if track.IsLive || r.port == 0 {
		return track, nil
	}

	// For VODs, route through our local HTTP proxy
	proxyURL := fmt.Sprintf("http://127.0.0.1:%d/stream?id=%s", r.port, url.QueryEscape(videoID))
	return ResolvedTrack{
		URL:    proxyURL,
		IsLive: false,
	}, nil
}

// pickAudioFormat selects the best audio-only format: audio/mp4 (itag 140 AAC)
// first as it plays in all three native backends; then any format that carries
// audio channels; then any format at all as a last resort.
func pickAudioFormat(formats youtube.FormatList) *youtube.Format {
	audio := formats.WithAudioChannels()
	if mp4 := audio.Type("audio/mp4"); len(mp4) > 0 {
		mp4.Sort()
		return &mp4[0]
	}
	if len(audio) > 0 {
		audio.Sort()
		return &audio[0]
	}
	if len(formats) > 0 {
		return &formats[0]
	}
	return nil
}

// ExpandPlaylist returns the ordered list of video IDs in the given playlist.
// Results are cached for playlistCacheTTL; pass forceRefresh=true to skip it.
func (r *Resolver) ExpandPlaylist(ctx context.Context, playlistID string, forceRefresh bool) ([]string, error) {
	playlistID = strings.TrimSpace(playlistID)
	if playlistID == "" {
		return nil, fmt.Errorf("radio: empty playlist id")
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	if !forceRefresh {
		if entry, ok := r.playlistCache[playlistID]; ok && len(entry.ids) > 0 && time.Since(entry.at) < playlistCacheTTL {
			return append([]string(nil), entry.ids...), nil
		}
	}

	pl, err := r.client.GetPlaylistContext(ctx, playlistID)
	if err != nil {
		return nil, fmt.Errorf("youtube: get playlist %s: %w", playlistID, err)
	}
	ids := make([]string, 0, len(pl.Videos))
	for _, v := range pl.Videos {
		if v != nil && v.ID != "" {
			ids = append(ids, v.ID)
		}
	}
	if len(ids) == 0 {
		return nil, fmt.Errorf("youtube: playlist %s has no playable videos", playlistID)
	}
	r.playlistCache[playlistID] = playlistEntry{ids: ids, at: time.Now()}
	return append([]string(nil), ids...), nil
}
