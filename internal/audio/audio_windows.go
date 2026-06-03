//go:build windows

package audio

import (
	"bufio"
	"fmt"
	"io"
	"log"
	"os/exec"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
)

type WindowsPlayer struct {
	mu       sync.Mutex
	cmd      *exec.Cmd
	stdin    io.WriteCloser
	emit     func(Event)
	ticker   *time.Ticker
	stopChan chan struct{}
	closed   bool
}

func New(emit func(Event)) (Player, error) {
	p := &WindowsPlayer{
		emit:     emit,
		stopChan: make(chan struct{}),
	}

	cmdScript := `
$ErrorActionPreference = "Stop"
[void][Windows.Media.Playback.MediaPlayer, Windows.Media.Playback, ContentType=WindowsRuntime]
[void][Windows.Media.Core.MediaSource, Windows.Media.Core, ContentType=WindowsRuntime]

$player = New-Object Windows.Media.Playback.MediaPlayer

# Natural end-of-track detection. Windows PowerShell cannot subscribe to WinRT
# events (Register-ObjectEvent on MediaPlayer.MediaEnded fails), and at EOS the
# PlaybackState merely collapses to "Paused" — indistinguishable from a user
# pause by state alone. But at a *natural* end the playback position equals the
# media's duration, whereas a user pause lands strictly short of it. So the
# existing 100ms "state" poll reports STATE:Ended exactly once when
# pos >= dur (state Paused), gated by $script:endedReported to avoid repeats.
$script:endedReported = $false

while ($line = [Console]::ReadLine()) {
    try {
        if ($line -like "play *") {
            $url = $line.Substring(5)
            Write-Host "PLAY_URL_LEN: $($url.Length)"
            $script:endedReported = $false
            $uri = New-Object System.Uri($url)
            $source = [Windows.Media.Core.MediaSource]::CreateFromUri($uri)
            $player.Source = $source
            $player.Play()
        } elseif ($line -eq "resume") {
            $player.Play()
        } elseif ($line -eq "pause") {
            $player.Pause()
        } elseif ($line -eq "stop") {
            $player.Pause()
        } elseif ($line -like "seek *") {
            $secStr = $line.Substring(5)
            $sec = [double]::Parse($secStr, [Globalization.CultureInfo]::InvariantCulture)
            if ($sec -lt 0.0) { $sec = 0.0 }
            $player.PlaybackSession.Position = [TimeSpan]::FromSeconds($sec)
        } elseif ($line -like "volume *") {
            $volStr = $line.Substring(7)
            $vol = [double]$volStr
            if ($vol -lt 0.0) { $vol = 0.0 }
            if ($vol -gt 1.0) { $vol = 1.0 }
            $player.Volume = $vol
        } elseif ($line -eq "state") {
            $sess = $player.PlaybackSession
            $st = $sess.PlaybackState
            $dur = $sess.NaturalDuration.TotalSeconds
            $pos = $sess.Position.TotalSeconds
            # Invariant-culture formatting so Go's strconv.ParseFloat never sees a
            # locale decimal comma.
            $ic = [Globalization.CultureInfo]::InvariantCulture
            $posStr = ([double]$pos).ToString("0.###", $ic)
            $durStr = ([double]$dur).ToString("0.###", $ic)
            if (-not $script:endedReported -and $dur -gt 0 -and $pos -ge ($dur - 0.5) -and "$st" -eq "Paused") {
                $script:endedReported = $true
                Write-Host "STATE:Ended|POS:$posStr|DUR:$durStr"
            } else {
                Write-Host "STATE:$st|POS:$posStr|DUR:$durStr"
            }
        } elseif ($line -eq "exit") {
            break
        }
    } catch {
        Write-Host "ERROR:$($_.Exception.Message)"
    }
}
`

	cmd := exec.Command("powershell", "-NoProfile", "-Command", cmdScript)
	cmd.SysProcAttr = &syscall.SysProcAttr{
		HideWindow:    true,
		CreationFlags: 0x08000000, // CREATE_NO_WINDOW
	}

	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, fmt.Errorf("failed to create stdin pipe: %w", err)
	}

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return nil, fmt.Errorf("failed to create stdout pipe: %w", err)
	}

	cmd.Stderr = log.Writer()

	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("failed to start powershell process: %w", err)
	}

	p.cmd = cmd
	p.stdin = stdin

	go p.readStdout(stdout)

	// Start active polling ticker (every 100ms)
	p.ticker = time.NewTicker(100 * time.Millisecond)
	go p.pollLoop()

	return p, nil
}

func (p *WindowsPlayer) readStdout(r io.Reader) {
	scanner := bufio.NewScanner(r)
	for scanner.Scan() {
		line := scanner.Text()
		if strings.HasPrefix(line, "STATE:") {
			// Format: STATE:<st>|POS:<pos>|DUR:<dur>
			stateStr, pos, dur := parseStateLine(strings.TrimPrefix(line, "STATE:"))
			p.handlePlayState(stateStr, pos, dur)
		} else if strings.HasPrefix(line, "ERROR:") {
			errStr := strings.TrimPrefix(line, "ERROR:")
			p.emit(Event{State: StateError, Err: errStr})
		} else {
			log.Printf("[Player stdout] %s", line)
		}
	}
}

// parseStateLine splits the "<st>|POS:<pos>|DUR:<dur>" payload (POS/DUR optional)
// into the WinRT playback-state name plus position/duration in seconds.
func parseStateLine(payload string) (state string, pos, dur float64) {
	parts := strings.Split(payload, "|")
	state = parts[0]
	for _, part := range parts[1:] {
		if v, ok := strings.CutPrefix(part, "POS:"); ok {
			pos, _ = strconv.ParseFloat(v, 64)
		} else if v, ok := strings.CutPrefix(part, "DUR:"); ok {
			dur, _ = strconv.ParseFloat(v, 64)
		}
	}
	return state, pos, dur
}

func (p *WindowsPlayer) handlePlayState(stateStr string, pos, dur float64) {
	switch stateStr {
	case "None":
		p.emit(Event{State: StateIdle})
	case "Opening", "Buffering":
		p.emit(Event{State: StateLoading})
	case "Playing":
		p.emit(Event{State: StatePlaying, Position: pos, Duration: dur})
	case "Paused":
		p.emit(Event{State: StatePaused, Position: pos, Duration: dur})
	case "Ended":
		// Natural end-of-track (drained from the MediaEnded flag by the poll).
		p.emit(Event{State: StateEnded})
	}
}

func (p *WindowsPlayer) pollLoop() {
	for {
		select {
		case <-p.ticker.C:
			p.mu.Lock()
			if p.stdin != nil {
				fmt.Fprintln(p.stdin, "state")
			}
			p.mu.Unlock()
		case <-p.stopChan:
			return
		}
	}
}

func (p *WindowsPlayer) Play(url string) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.closed || p.stdin == nil {
		return fmt.Errorf("player not initialized or closed")
	}
	_, err := fmt.Fprintf(p.stdin, "play %s\n", url)
	return err
}

func (p *WindowsPlayer) Resume() error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.closed || p.stdin == nil {
		return fmt.Errorf("player not initialized or closed")
	}
	_, err := fmt.Fprintln(p.stdin, "resume")
	return err
}

func (p *WindowsPlayer) Pause() error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.closed || p.stdin == nil {
		return fmt.Errorf("player not initialized or closed")
	}
	_, err := fmt.Fprintln(p.stdin, "pause")
	return err
}

func (p *WindowsPlayer) Stop() error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.closed || p.stdin == nil {
		return fmt.Errorf("player not initialized or closed")
	}
	_, err := fmt.Fprintln(p.stdin, "stop")
	return err
}

func (p *WindowsPlayer) Seek(seconds float64) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.closed || p.stdin == nil {
		return fmt.Errorf("player not initialized or closed")
	}
	if seconds < 0 {
		seconds = 0
	}
	_, err := fmt.Fprintf(p.stdin, "seek %f\n", seconds)
	return err
}

func (p *WindowsPlayer) SetVolume(v float64) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.closed || p.stdin == nil {
		return fmt.Errorf("player not initialized or closed")
	}
	if v < 0.0 {
		v = 0.0
	} else if v > 1.0 {
		v = 1.0
	}
	_, err := fmt.Fprintf(p.stdin, "volume %f\n", v)
	return err
}

func (p *WindowsPlayer) Close() error {
	p.mu.Lock()
	if p.closed {
		p.mu.Unlock()
		return nil
	}
	p.closed = true
	p.mu.Unlock()

	if p.ticker != nil {
		p.ticker.Stop()
	}
	close(p.stopChan)

	p.mu.Lock()
	defer p.mu.Unlock()
	if p.stdin != nil {
		fmt.Fprintln(p.stdin, "exit")
		p.stdin.Close()
		p.stdin = nil
	}
	if p.cmd != nil {
		p.cmd.Process.Kill()
		p.cmd.Wait()
		p.cmd = nil
	}
	return nil
}
