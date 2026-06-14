//go:build linux && !cgo

package platform

// No-cgo fallback (e.g. CGO_ENABLED=0 server builds): no gtk4-layer-shell. The
// layerShellSupported probe keeps its default (false) so detection lands on
// BackendWaylandPlain, and these primitives are no-ops.

func layerShellInitWindow(win uintptr)                    {}
func layerShellDock(win uintptr, bottom bool, h int)      {}
func layerShellSetExclusiveZone(win uintptr, zone int)    {}
