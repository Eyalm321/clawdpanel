//go:build linux && cgo

package platform

/*
#cgo pkg-config: gtk4
#include <gtk/gtk.h>

// Event-driven hover detection for the bar, replacing the global cursor poll
// (xdotool getmouselocation), which FREEZES under XWayland on a Wayland session.
// A GtkEventControllerMotion on the top-level window receives pointer crossing
// events for the app's OWN surface — an independent channel that keeps working
// while the global query is stuck (verified live on GNOME Wayland). We track the
// controller's "contains-pointer" property (true when the pointer is over the
// window OR a descendant, e.g. the WebKitWebView child) so moving between the
// window chrome and the webview doesn't emit a spurious leave; capture phase so
// the parent sees the crossing first.
extern void goBarHover(int over);

static void on_contains_pointer(GObject *c, GParamSpec *p, gpointer u) {
    gboolean over = gtk_event_controller_motion_contains_pointer(GTK_EVENT_CONTROLLER_MOTION(c));
    goBarHover(over ? 1 : 0);
}

static void attach_hover(uintptr_t wp) {
    GtkWidget *w = (GtkWidget *) wp; // GtkWindow* is-a GtkWidget*
    if (!w) return;
    GtkEventController *c = gtk_event_controller_motion_new();
    gtk_event_controller_set_propagation_phase(c, GTK_PHASE_CAPTURE);
    g_signal_connect(c, "notify::contains-pointer", G_CALLBACK(on_contains_pointer), NULL);
    gtk_widget_add_controller(w, c);
}
*/
import "C"

import "sync/atomic"

// hoverCallback is invoked from the GTK main thread on every hover transition.
// It must not block or touch GTK directly (the reveal Controller is goroutine-safe
// and only flips state), so this is a plain stored func.
var hoverCallback atomic.Pointer[func(bool)]

// RegisterHoverCallback sets the function the native motion controller calls when
// the pointer enters (true) or leaves (false) the bar window. Call before
// AttachHoverController.
func RegisterHoverCallback(cb func(bool)) {
	hoverCallback.Store(&cb)
}

// AttachHoverController wires a GtkEventControllerMotion onto the bar's GtkWindow
// (its native pointer from window.NativeWindow(), captured before app.go swaps it
// for the X11 id). Must run on the GTK main thread. No-op on a zero handle.
func AttachHoverController(gtkWin uintptr) {
	if gtkWin == 0 {
		return
	}
	C.attach_hover(C.uintptr_t(gtkWin))
}

//export goBarHover
func goBarHover(over C.int) {
	if cb := hoverCallback.Load(); cb != nil && *cb != nil {
		(*cb)(over == 1)
	}
}
