//go:build linux && !cgo

package platform

// No-cgo fallback (CGO_ENABLED=0 server builds): no GTK motion controller, so
// event-driven hover is unavailable. The reveal machine falls back to the poll.

func RegisterHoverCallback(cb func(bool)) {}
func AttachHoverController(gtkWin uintptr)  {}
