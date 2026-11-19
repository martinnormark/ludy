//! `--click`: post real mouse events to this app's own window, so buttons
//! can be exercised without a hand on the mouse. The events go through
//! AppKit's queue like any other, and posting to our own process needs no
//! Accessibility permission.

// The `objc` macros test a cfg that newer compilers do not know.
#![allow(unexpected_cfgs)]

use objc::runtime::Object;
use objc::{class, msg_send, sel, sel_impl};

#[repr(C)]
#[derive(Clone, Copy)]
struct NSPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NSRect {
    origin: NSPoint,
    size: NSPoint,
}

const LEFT_DOWN: u64 = 1;
const LEFT_UP: u64 = 2;
const RIGHT_DOWN: u64 = 3;
const RIGHT_UP: u64 = 4;
const MOVED: u64 = 5;

/// Post one mouse event at (x, y) in window points, origin top-left.
/// Must run on the main thread.
unsafe fn post(kind: u64, x: f32, y: f32, clicks: isize) {
    let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
    let windows: *mut Object = msg_send![app, windows];
    let count: usize = msg_send![windows, count];
    for i in 0..count {
        let window: *mut Object = msg_send![windows, objectAtIndex: i];
        let visible: bool = msg_send![window, isVisible];
        let content: *mut Object = msg_send![window, contentView];
        if !visible || content.is_null() {
            continue;
        }
        let bounds: NSRect = msg_send![content, bounds];
        if bounds.size.x < 400.0 {
            continue;
        }
        let number: isize = msg_send![window, windowNumber];
        let info: *mut Object = msg_send![class!(NSProcessInfo), processInfo];
        let now: f64 = msg_send![info, systemUptime];
        // AppKit's origin is the bottom-left corner.
        let location = NSPoint { x: x as f64, y: bounds.size.y - y as f64 };
        let nil: *mut Object = std::ptr::null_mut();
        let event: *mut Object = msg_send![class!(NSEvent), mouseEventWithType: kind
            location: location
            modifierFlags: 0u64
            timestamp: now
            windowNumber: number
            context: nil
            eventNumber: 0isize
            clickCount: clicks
            pressure: 1.0f32];
        let _: () = msg_send![app, postEvent: event atStart: false];
        return;
    }
}

/// kind: 0 = click, 1 = right-click, 2 = double-click.
pub fn mouse_move(x: f32, y: f32) {
    unsafe { post(MOVED, x, y, 0) }
}

pub fn mouse_down(x: f32, y: f32, kind: u8, nth: isize) {
    unsafe { post(if kind == 1 { RIGHT_DOWN } else { LEFT_DOWN }, x, y, nth) }
}

pub fn mouse_up(x: f32, y: f32, kind: u8, nth: isize) {
    unsafe { post(if kind == 1 { RIGHT_UP } else { LEFT_UP }, x, y, nth) }
}
