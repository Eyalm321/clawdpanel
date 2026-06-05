//go:build darwin

package platform

/*
#cgo darwin CFLAGS: -x objective-c -fobjc-arc
#cgo darwin LDFLAGS: -framework Cocoa -framework ApplicationServices
#import <Cocoa/Cocoa.h>
#import <ApplicationServices/ApplicationServices.h>

// Updated by FullscreenWatcher whenever the active Space changes. Read
// atomically from any thread by Go via platformIsFullScreenActive. A plain
// int read is atomic on aarch64/x86_64 when aligned, which it is here.
static volatile int g_fullscreenActive = 0;

@interface FullscreenWatcher : NSObject
+ (instancetype)shared;
- (void)start;
- (void)refresh;
@end

@implementation FullscreenWatcher

+ (instancetype)shared {
    static FullscreenWatcher* s = nil;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        s = [[FullscreenWatcher alloc] init];
    });
    return s;
}

- (void)start {
    NSNotificationCenter* nc = [[NSWorkspace sharedWorkspace] notificationCenter];
    [nc removeObserver:self];
    [nc addObserver:self
           selector:@selector(spaceChanged:)
               name:NSWorkspaceActiveSpaceDidChangeNotification
             object:nil];
    [self refresh];
}

- (void)spaceChanged:(NSNotification*)notif {
    [self refresh];
}

// refresh queries the frontmost app's main window for AXFullScreen and stores
// the result in g_fullscreenActive. Runs on whichever thread invoked it —
// NSWorkspaceActiveSpaceDidChangeNotification is delivered on the main
// thread, which is where AX calls should originate, so this is safe.
- (void)refresh {
    BOOL fs = NO;
    NSRunningApplication* front = [[NSWorkspace sharedWorkspace] frontmostApplication];
    if (front) {
        pid_t pid = front.processIdentifier;
        AXUIElementRef app = AXUIElementCreateApplication(pid);
        if (app) {
            // Tight timeout so an unresponsive frontmost app can't stall the
            // main thread for the full 6s default.
            AXUIElementSetMessagingTimeout(app, 0.5f);

            CFTypeRef mainWin = NULL;
            if (AXUIElementCopyAttributeValue(app, kAXMainWindowAttribute, &mainWin) == kAXErrorSuccess && mainWin) {
                CFTypeRef fsAttr = NULL;
                if (AXUIElementCopyAttributeValue((AXUIElementRef)mainWin, CFSTR("AXFullScreen"), &fsAttr) == kAXErrorSuccess && fsAttr) {
                    if (CFGetTypeID(fsAttr) == CFBooleanGetTypeID() && CFBooleanGetValue(fsAttr)) {
                        fs = YES;
                    }
                    CFRelease(fsAttr);
                }
                CFRelease(mainWin);
            }
            CFRelease(app);
        }
    }
    g_fullscreenActive = fs ? 1 : 0;
}

@end

static int platformIsFullScreenActive(void) {
    return g_fullscreenActive;
}

static void platformStartFullscreenWatcher(void) {
    // Always hop to main — NSWorkspace notifications fire on main and AX
    // queries should originate there.
    dispatch_async(dispatch_get_main_queue(), ^{
        [[FullscreenWatcher shared] start];
    });
}
*/
import "C"

import "sync"

var fsWatcherOnce sync.Once

func startFullscreenWatcher() {
	fsWatcherOnce.Do(func() {
		C.platformStartFullscreenWatcher()
	})
}

// IsFullScreenActive reports whether the frontmost app's main window is in
// macOS native fullscreen mode. Updated by an NSWorkspace observer on each
// Space change; the very first call lazily starts the observer. Cheap to
// call frequently — just an atomic int read after init.
//
// The mon argument (claudebar's monitor) is accepted for parity with the
// Windows implementation, which scopes detection to a single display, but is
// ignored here: macOS fullscreen is a per-Space concept the observer already
// tracks globally, not a per-display bounds check.
func IsFullScreenActive(_ MonitorInfo) bool {
	startFullscreenWatcher()
	return C.platformIsFullScreenActive() != 0
}
