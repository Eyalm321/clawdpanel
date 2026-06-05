//go:build windows

package audio

import (
	"context"
	"log"
	"os/exec"
	"testing"
	"time"

	"clawdpanel/internal/radio"
)

func TestWinRTPlayer_Real(t *testing.T) {
	p, err := New(func(ev Event) {
		log.Printf("[Player Event Callback] State=%s, Err=%s", ev.State, ev.Err)
	})
	if err != nil {
		t.Fatalf("Failed to initialize player: %v", err)
	}
	defer p.Close()

	log.Printf("Testing playing standard MP3 stream...")
	err = p.Play("http://icecast.radiofrance.fr/fip-midfi.mp3")
	if err != nil {
		t.Fatalf("Play failed: %v", err)
	}

	time.Sleep(5 * time.Second)

	log.Printf("Testing volume adjustment to 0.2...")
	err = p.SetVolume(0.2)
	if err != nil {
		t.Fatalf("SetVolume failed: %v", err)
	}
	time.Sleep(2 * time.Second)

	log.Printf("Testing volume adjustment to 0.8...")
	err = p.SetVolume(0.8)
	if err != nil {
		t.Fatalf("SetVolume failed: %v", err)
	}
	time.Sleep(2 * time.Second)

	log.Printf("Testing Pause...")
	err = p.Pause()
	if err != nil {
		t.Fatalf("Pause failed: %v", err)
	}
	time.Sleep(2 * time.Second)

	log.Printf("Testing Play again (resume)...")
	err = p.Play("http://icecast.radiofrance.fr/fip-midfi.mp3")
	if err != nil {
		t.Fatalf("Play failed: %v", err)
	}
	time.Sleep(3 * time.Second)
}

func TestRealWinRT_MediaPlayer(t *testing.T) {
	// 1. Resolve a live YouTube stream URL
	resolver := radio.New()
	videoID := "EWrX250Zhko" // Synthwave Radio live stream
	log.Printf("Resolving YouTube stream URL for video %s...", videoID)
	track, err := resolver.Resolve(context.Background(), videoID, false)
	if err != nil {
		t.Fatalf("Failed to resolve stream URL: %v", err)
	}
	hlsURL := track.URL
	log.Printf("Resolved YouTube URL: %s", hlsURL)

	// 2. Run PowerShell with WinRT MediaPlayer in background
	cmdStr := `
		try {
			# Load WinRT assemblies
			[void][Windows.Media.Playback.MediaPlayer, Windows.Media.Playback, ContentType=WindowsRuntime]
			[void][Windows.Media.Core.MediaSource, Windows.Media.Core, ContentType=WindowsRuntime]
			
			$uri = New-Object System.Uri("` + hlsURL + `")
			$source = [Windows.Media.Core.MediaSource]::CreateFromUri($uri)
			
			$player = New-Object Windows.Media.Playback.MediaPlayer
			$player.Source = $source
			$player.Play()
			
			Write-Host "WinRT Player created successfully. Playing for 15 seconds..."
			for ($i = 0; $i -lt 15; $i++) {
				Start-Sleep -Seconds 1
				$state = $player.PlaybackSession.PlaybackState
				Write-Host "Second $i - State: $state - Volume: $($player.Volume)"
			}
			$player.Pause()
		} catch {
			Write-Host "Error occurred: $_"
			Write-Host $_.ScriptStackTrace
		}
	`

	cmd := exec.Command("powershell", "-NoProfile", "-Command", cmdStr)
	cmd.Stdout = log.Writer()
	cmd.Stderr = log.Writer()

	log.Printf("=== Playing HLS via WinRT MediaPlayer in PowerShell ===")
	err = cmd.Run()
	if err != nil {
		t.Fatalf("PowerShell command failed: %v", err)
	}
}
