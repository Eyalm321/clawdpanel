package platform

// MonitorInfo is the app-facing monitor descriptor, exposed to the frontend.
// Fields are identical across platforms; per-OS implementations populate them
// from their native display APIs.
type MonitorInfo struct {
	Index     int     `json:"index"`
	Left      int32   `json:"left"`      // physical pixels
	Top       int32   `json:"top"`       // physical pixels
	Width     int     `json:"width"`     // logical pixels
	Height    int     `json:"height"`    // logical pixels
	PhysWidth int     `json:"physWidth"` // physical pixels (use for OS-native sizing calls)
	DpiScale  float64 `json:"dpiScale"`  // e.g. 1.25 at 125%
	IsPrimary bool    `json:"isPrimary"`
	Name      string  `json:"name"`
	// WorkTopOffset is the number of points/pixels between the monitor's true
	// top edge (Top) and where the bar's *resting* top lives. On macOS this is
	// the menu bar height (the menu bar is non-removable and always above the
	// bar's window level), so the bar sits at Top + WorkTopOffset. On Windows
	// and Linux this is 0 — the bar occupies the very top of the monitor and
	// the existing AppBar mechanism reserves the strip. Used by the slide
	// animation target and the hover-detection hit box so they agree with
	// where DockToMonitor actually places the window.
	WorkTopOffset int `json:"workTopOffset"`
	// DockEdge is which edge of this monitor the bar should dock to: "top"
	// (default; empty string means top) or "bottom". Linux sets it per
	// monitor: X11 struts can only reserve space measured from the ROOT
	// screen edges, so on stacked layouts a monitor with another above it
	// can only get true space reservation along its bottom edge. Windows
	// and macOS always dock top and leave this empty.
	DockEdge string `json:"dockEdge"`
}

// PushdownStats contains diagnostic information about macOS window pushdown.
type PushdownStats struct {
	Enabled           bool   `json:"enabled"`
	Trusted           bool   `json:"trusted"`
	ObservedApps      int    `json:"observedApps"`
	PushesThisSession int    `json:"pushesThisSession"`
	LastError         string `json:"lastError"`
}

