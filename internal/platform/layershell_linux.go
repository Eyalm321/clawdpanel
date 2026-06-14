//go:build linux && cgo

package platform

/*
#cgo pkg-config: gtk4
#include <gtk/gtk.h>
#include <dlfcn.h>
#include <stdint.h>

// gtk4-layer-shell is loaded at runtime via dlopen (not link-time) so the single
// binary still starts on systems without the library — there layer-shell is
// simply reported unsupported and we fall back to BackendWaylandPlain. The enums
// and signatures are redeclared here so the -devel header isn't a build
// requirement either. The window handle crosses the boundary as a uintptr_t and
// is cast to GtkWindow* in C, so the Go side never converts uintptr->unsafe.Pointer.
enum { LS_LAYER_TOP = 2 };
enum { LS_EDGE_LEFT = 0, LS_EDGE_RIGHT = 1, LS_EDGE_TOP = 2, LS_EDGE_BOTTOM = 3 };

typedef int  (*ls_is_supported_t)(void);
typedef void (*ls_init_t)(GtkWindow*);
typedef void (*ls_set_layer_t)(GtkWindow*, int);
typedef void (*ls_set_anchor_t)(GtkWindow*, int, int);
typedef void (*ls_set_zone_t)(GtkWindow*, int);
typedef void (*ls_set_ns_t)(GtkWindow*, const char*);

static void*             ls_h          = NULL;
static int               ls_tried      = 0;
static ls_is_supported_t p_supported   = NULL;
static ls_init_t         p_init        = NULL;
static ls_set_layer_t    p_set_layer   = NULL;
static ls_set_anchor_t   p_set_anchor  = NULL;
static ls_set_zone_t     p_set_zone    = NULL;
static ls_set_ns_t       p_set_ns      = NULL;

static int ls_load(void) {
    if (ls_tried) return ls_h != NULL && p_init != NULL;
    ls_tried = 1;
    ls_h = dlopen("libgtk4-layer-shell.so.0", RTLD_LAZY | RTLD_GLOBAL);
    if (!ls_h) ls_h = dlopen("libgtk4-layer-shell.so", RTLD_LAZY | RTLD_GLOBAL);
    if (!ls_h) return 0;
    p_supported  = (ls_is_supported_t) dlsym(ls_h, "gtk_layer_is_supported");
    p_init       = (ls_init_t)         dlsym(ls_h, "gtk_layer_init_for_window");
    p_set_layer  = (ls_set_layer_t)    dlsym(ls_h, "gtk_layer_set_layer");
    p_set_anchor = (ls_set_anchor_t)   dlsym(ls_h, "gtk_layer_set_anchor");
    p_set_zone   = (ls_set_zone_t)     dlsym(ls_h, "gtk_layer_set_exclusive_zone");
    p_set_ns     = (ls_set_ns_t)       dlsym(ls_h, "gtk_layer_set_namespace");
    return p_supported && p_init && p_set_layer && p_set_anchor && p_set_zone;
}

// cls_supported requires gtk_init() to have run (gtk_layer_is_supported asserts
// it). Callers resolve this only post-init.
static int cls_supported(void) {
    if (!ls_load()) return 0;
    return p_supported() ? 1 : 0;
}

static void cls_init(uintptr_t wp) {
    GtkWindow* w = (GtkWindow*) wp;
    if (!w || !ls_load()) return;
    p_init(w);
    if (p_set_ns) p_set_ns(w, "clawdpanel");
    p_set_layer(w, LS_LAYER_TOP);
}

// cls_dock anchors the bar to the full width of its monitor's top (or bottom)
// edge and reserves barHeight via the exclusive zone — the layer-shell analogue
// of _NET_WM_STRUT_PARTIAL, but compositor-managed (no positioning math).
static void cls_dock(uintptr_t wp, int bottom, int barHeight) {
    GtkWindow* w = (GtkWindow*) wp;
    if (!w || !ls_load()) return;
    p_set_anchor(w, LS_EDGE_LEFT, 1);
    p_set_anchor(w, LS_EDGE_RIGHT, 1);
    p_set_anchor(w, LS_EDGE_TOP, bottom ? 0 : 1);
    p_set_anchor(w, LS_EDGE_BOTTOM, bottom ? 1 : 0);
    p_set_zone(w, barHeight);
}

static void cls_set_zone(uintptr_t wp, int zone) {
    GtkWindow* w = (GtkWindow*) wp;
    if (!w || !ls_load()) return;
    p_set_zone(w, zone);
}
*/
import "C"

func init() {
	// Wire the real layer-shell capability probe into the backend detector. Safe
	// because Backend() resolves only post-gtk_init (domReady / window ops), never
	// from main() — which uses WantsXWayland() (env-only) instead.
	layerShellSupported = func() bool { return C.cls_supported() == 1 }
}

// layerShellInitWindow converts the GTK window (its native pointer) into a
// layer-shell surface. MUST run before the window is realized/mapped.
func layerShellInitWindow(win uintptr) {
	if win == 0 {
		return
	}
	C.cls_init(C.uintptr_t(win))
}

// layerShellDock anchors + reserves space for the bar on its monitor edge.
func layerShellDock(win uintptr, bottom bool, barHeight int) {
	if win == 0 {
		return
	}
	var b C.int
	if bottom {
		b = 1
	}
	C.cls_dock(C.uintptr_t(win), b, C.int(barHeight))
}

// layerShellSetExclusiveZone toggles the reserved space (barHeight to show the
// dock, 0 to release it) — the hook for a future layer-shell auto-hide.
func layerShellSetExclusiveZone(win uintptr, zone int) {
	if win == 0 {
		return
	}
	C.cls_set_zone(C.uintptr_t(win), C.int(zone))
}
