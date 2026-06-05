package station

import (
	"testing"

	"clawdpanel/internal/config"
)

func TestParseItem(t *testing.T) {
	tests := []struct {
		name     string
		input    string
		wantKind config.StationItemKind
		wantID   string
		wantErr  bool
	}{
		{"bare video id", "YmQ7jRgf4f0", config.ItemVideo, "YmQ7jRgf4f0", false},
		{"bare playlist id", "PLAbcdEfGhIjKlMnOpQrSt", config.ItemPlaylist, "PLAbcdEfGhIjKlMnOpQrSt", false},
		{"watch url", "https://www.youtube.com/watch?v=EWrX250Zhko", config.ItemVideo, "EWrX250Zhko", false},
		{"watch url https no www", "https://youtube.com/watch?v=EWrX250Zhko", config.ItemVideo, "EWrX250Zhko", false},
		{"watch url www no scheme", "www.youtube.com/watch?v=EWrX250Zhko", config.ItemVideo, "EWrX250Zhko", false},
		{"watch url bare host", "youtube.com/watch?v=EWrX250Zhko", config.ItemVideo, "EWrX250Zhko", false},
		{"music subdomain", "https://music.youtube.com/watch?v=EWrX250Zhko", config.ItemVideo, "EWrX250Zhko", false},
		{"youtu.be short", "https://youtu.be/EWrX250Zhko", config.ItemVideo, "EWrX250Zhko", false},
		{"youtu.be with si param", "https://youtu.be/EWrX250Zhko?si=AbCdEfGhIj", config.ItemVideo, "EWrX250Zhko", false},
		{"shorts", "https://www.youtube.com/shorts/EWrX250Zhko", config.ItemVideo, "EWrX250Zhko", false},
		{"embed", "https://www.youtube.com/embed/EWrX250Zhko", config.ItemVideo, "EWrX250Zhko", false},
		{"v form", "https://www.youtube.com/v/EWrX250Zhko", config.ItemVideo, "EWrX250Zhko", false},
		{"live url", "https://www.youtube.com/live/EWrX250Zhko", config.ItemVideo, "EWrX250Zhko", false},
		{"watch with list -> playlist wins", "https://www.youtube.com/watch?v=EWrX250Zhko&list=PLAbcdEfGhIjKlMnOpQrSt", config.ItemPlaylist, "PLAbcdEfGhIjKlMnOpQrSt", false},
		{"youtu.be with list -> playlist wins", "https://youtu.be/EWrX250Zhko?list=PLAbcdEfGhIjKlMnOpQrSt", config.ItemPlaylist, "PLAbcdEfGhIjKlMnOpQrSt", false},
		{"playlist url", "https://www.youtube.com/playlist?list=PLAbcdEfGhIjKlMnOpQrSt", config.ItemPlaylist, "PLAbcdEfGhIjKlMnOpQrSt", false},
		{"playlist url www no scheme", "www.youtube.com/playlist?list=PLAbcdEfGhIjKlMnOpQrSt", config.ItemPlaylist, "PLAbcdEfGhIjKlMnOpQrSt", false},
		{"playlist url with extra param", "https://www.youtube.com/playlist?list=PLAbcdEfGhIjKlMnOpQrSt&si=xyz", config.ItemPlaylist, "PLAbcdEfGhIjKlMnOpQrSt", false},
		{"list-first query", "https://www.youtube.com/watch?list=PLAbcdEfGhIjKlMnOpQrSt", config.ItemPlaylist, "PLAbcdEfGhIjKlMnOpQrSt", false},
		{"whitespace trimmed", "  YmQ7jRgf4f0  ", config.ItemVideo, "YmQ7jRgf4f0", false},
		{"empty", "", "", "", true},
		{"garbage", "hello", "", "", true},
		{"12-char token (neither video nor playlist)", "abcdefghijkl", "", "", true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := ParseItem(tt.input)
			if tt.wantErr {
				if err == nil {
					t.Fatalf("ParseItem(%q) = %+v, want error", tt.input, got)
				}
				return
			}
			if err != nil {
				t.Fatalf("ParseItem(%q) unexpected error: %v", tt.input, err)
			}
			if got.Kind != tt.wantKind {
				t.Errorf("ParseItem(%q) kind = %q, want %q", tt.input, got.Kind, tt.wantKind)
			}
			if got.ID != tt.wantID {
				t.Errorf("ParseItem(%q) id = %q, want %q", tt.input, got.ID, tt.wantID)
			}
		})
	}
}

func TestHasMultipleTracks(t *testing.T) {
	tests := []struct {
		name  string
		items []config.StationItem
		want  bool
	}{
		{"empty station", nil, false},
		{"single video", []config.StationItem{{Kind: config.ItemVideo, ID: "YmQ7jRgf4f0"}}, false},
		{"single livestream", []config.StationItem{{Kind: config.ItemLivestream, ID: "EWrX250Zhko"}}, false},
		{"single playlist", []config.StationItem{{Kind: config.ItemPlaylist, ID: "PLAbcdEfGhIjKlMnOpQrSt"}}, true},
		{"two videos", []config.StationItem{
			{Kind: config.ItemVideo, ID: "YmQ7jRgf4f0"},
			{Kind: config.ItemVideo, ID: "EWrX250Zhko"},
		}, true},
		// The real-world GTA 5 Radio case: saved with a stale "video" kind but the
		// Raw URL carries a list=, so the player expands it to many tracks.
		{"stale video kind with list= in raw", []config.StationItem{{
			Kind: config.ItemVideo,
			ID:   "6TnV43UWoqk",
			Raw:  "https://www.youtube.com/watch?v=6TnV43UWoqk&list=PLLvWV__Bn2_PwR92FfrxjsZCAM7zyxzze",
		}}, true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := HasMultipleTracks(config.StationConfig{Name: "S", Items: tt.items})
			if got != tt.want {
				t.Errorf("HasMultipleTracks(%+v) = %v, want %v", tt.items, got, tt.want)
			}
		})
	}
}
