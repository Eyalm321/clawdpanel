package radio

import (
	"bytes"
	"encoding/xml"
	"os"
	"strconv"
	"testing"
)

// testdata/live-dynamic.mpd is a captured YouTube live DASH manifest
// (4xDzrJKXOOY, 5s segments), with each SegmentList shrunk to its last 12
// entries. Structure preserved: one attributed SegmentList carrying the
// timeline (presentationTimeOffset/startNumber/timescale + <S> entries) and
// one bare SegmentList of <SegmentURL>s per Representation, plus video
// AdaptationSets.
func loadFixture(t *testing.T) []byte {
	t.Helper()
	b, err := os.ReadFile("testdata/live-dynamic.mpd")
	if err != nil {
		t.Fatalf("read fixture: %v", err)
	}
	return b
}

// fixture facts (from the captured manifest)
const (
	fixturePTO        = 7518965733
	fixtureStart      = 1503782
	fixtureSegMs      = 5000
	fixtureSegs       = 12
	fixtureFirstSeg   = 1506650 // first <SegmentURL media="sq/N/..."> kept in the shrunken lists
	fixtureLastSegURL = `media="sq/1506661/`
)

func TestStaticizeLiveMPD(t *testing.T) {
	out, err := staticizeLiveMPD(loadFixture(t))
	if err != nil {
		t.Fatalf("staticize: %v", err)
	}

	if !utf8XMLWellFormed(out) {
		t.Fatalf("output is not well-formed XML:\n%s", out)
	}
	if !bytes.Contains(out, []byte(`type="static"`)) || bytes.Contains(out, []byte(`type="dynamic"`)) {
		t.Errorf("MPD type not rewritten to static")
	}
	for _, attr := range []string{"minimumUpdatePeriod", "timeShiftBufferDepth", "availabilityStartTime"} {
		if bytes.Contains(out, []byte(attr)) {
			t.Errorf("dynamic attribute %s survived", attr)
		}
	}
	if bytes.Contains(out, []byte(`mimeType="video/`)) {
		t.Errorf("video AdaptationSet survived")
	}

	// Window (60s) is wider than the fixture's 12 segments: nothing trimmed,
	// PTO and startNumber must pass through unchanged.
	if want := []byte(`presentationTimeOffset="` + strconv.Itoa(fixturePTO) + `"`); !bytes.Contains(out, want) {
		t.Errorf("untrimmed manifest must keep upstream PTO %d", fixturePTO)
	}
	if want := []byte(`startNumber="` + strconv.Itoa(fixtureStart) + `"`); !bytes.Contains(out, want) {
		t.Errorf("untrimmed manifest must keep upstream startNumber %d", fixtureStart)
	}
	if want := []byte(`mediaPresentationDuration="PT60.000S"`); !bytes.Contains(out, want) {
		t.Errorf("mediaPresentationDuration: want PT60.000S, manifest header: %s", out[:200])
	}
}

func TestStaticizeLiveMPDTrim(t *testing.T) {
	// 20s window over 5s segments: keep the last 4, drop the leading 8.
	out, err := staticizeLiveMPDWindow(loadFixture(t), 20*1000)
	if err != nil {
		t.Fatalf("staticize: %v", err)
	}
	if !utf8XMLWellFormed(out) {
		t.Fatalf("output is not well-formed XML:\n%s", out)
	}

	dropped := fixtureSegs - 4
	wantPTO := fixturePTO + dropped*fixtureSegMs
	wantStart := fixtureStart + dropped
	if want := []byte(`presentationTimeOffset="` + strconv.Itoa(wantPTO) + `"`); !bytes.Contains(out, want) {
		t.Errorf("PTO not shifted by dropped lead: want %d\nseglist: %s", wantPTO, firstSegListTag(out))
	}
	if want := []byte(`startNumber="` + strconv.Itoa(wantStart) + `"`); !bytes.Contains(out, want) {
		t.Errorf("startNumber not shifted: want %d\nseglist: %s", wantStart, firstSegListTag(out))
	}
	if want := []byte(`mediaPresentationDuration="PT20.000S"`); !bytes.Contains(out, want) {
		t.Errorf("mediaPresentationDuration: want PT20.000S")
	}

	// Every per-Representation URL list trimmed to 4, keeping the freshest tail.
	urls := reSegURL.FindAll(out, -1)
	if len(urls) != 4*2 { // two audio Representations
		t.Errorf("want 4 SegmentURLs per Representation (8 total), got %d", len(urls))
	}
	if !bytes.Contains(out, []byte(fixtureLastSegURL)) {
		t.Errorf("freshest segment lost from the trimmed window")
	}
	if got := len(reSEntry.FindAll(out, -1)); got != 4 {
		t.Errorf("want 4 timeline <S> entries, got %d", got)
	}
}

func TestStaticizeLiveMPDRejectsNonDynamic(t *testing.T) {
	if _, err := staticizeLiveMPD([]byte(`<MPD type="static"></MPD>`)); err == nil {
		t.Fatal("expected error for non-dynamic MPD")
	}
}

// firstSegListTag returns the first attributed <SegmentList ...> tag, for
// error messages.
func firstSegListTag(b []byte) string {
	return string(reSegListBlock.Find(b))[:120]
}

// utf8XMLWellFormed checks the bytes parse as XML.
func utf8XMLWellFormed(b []byte) bool {
	dec := xml.NewDecoder(bytes.NewReader(b))
	for {
		_, err := dec.Token()
		if err != nil {
			return err.Error() == "EOF"
		}
	}
}
