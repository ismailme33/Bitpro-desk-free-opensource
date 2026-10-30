use super::{absolute_move, post, relative_move, Msg, Screen};
use crate::input::*;
use core_foundation::{
    base::{CFType, TCFType},
    boolean::CFBoolean,
    dictionary::{CFDictionary, CFDictionaryRef},
    mach_port::{CFMachPort, CFMachPortInvalidate, CFMachPortRef},
    runloop::{kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopRef, CFRunLoopStop},
    string::{CFString, CFStringRef},
};
use core_graphics::{display::CGDisplay, geometry::CGPoint};
use hbb_common::libc;
use std::{
    ffi::c_void,
    fs::File,
    os::unix::io::AsRawFd,
    path::Path,
    ptr::null_mut,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::Duration,
};

pub const SUPPORTED: bool = true;

const HID_EVENT_TAP: u32 = 0;
const HEAD_INSERT_EVENT_TAP: u32 = 0;
const DEFAULT_TAP_OPTIONS: u32 = 0;
const HID_SYSTEM_STATE: i32 = 1;
const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

const LEFT_DOWN: u32 = 1;
const LEFT_UP: u32 = 2;
const RIGHT_DOWN: u32 = 3;
const RIGHT_UP: u32 = 4;
const MOVED: u32 = 5;
const LEFT_DRAGGED: u32 = 6;
const RIGHT_DRAGGED: u32 = 7;
const ROTATE: u32 = 18;
const BEGIN_GESTURE: u32 = 19;
const END_GESTURE: u32 = 20;
const SCROLL: u32 = 22;
const OTHER_DOWN: u32 = 25;
const OTHER_UP: u32 = 26;
const OTHER_DRAGGED: u32 = 27;
const GESTURE: u32 = 29;
const MAGNIFY: u32 = 30;
const SWIPE: u32 = 31;
const SMART_MAGNIFY: u32 = 32;
const PRESSURE: u32 = 34;

const BUTTON_NUMBER: u32 = 3;
const DELTA_X: u32 = 4;
const DELTA_Y: u32 = 5;
const SUBTYPE: u32 = 7;
const SCROLL_Y: u32 = 11;
const SCROLL_X: u32 = 12;
const SCROLL_CONTINUOUS: u32 = 88;
const SCROLL_POINT_Y: u32 = 96;
const SCROLL_POINT_X: u32 = 97;

const TABLET_POINT: i64 = 1;

type TapCallback = extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> *mut c_void;

extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        mask: u64,
        callback: TapCallback,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetIntegerValueField(event: *mut c_void, field: u32) -> i64;
    fn CGEventGetDoubleValueField(event: *mut c_void, field: u32) -> f64;
    fn CGEventGetLocation(event: *const c_void) -> CGPoint;
    fn CGEventSourceButtonState(state: i32, button: u32) -> bool;
    fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
}

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static HIDDEN: AtomicBool = AtomicBool::new(false);
static TAP: AtomicUsize = AtomicUsize::new(0);
static RUN_LOOP: AtomicUsize = AtomicUsize::new(0);

pub fn local_screens() -> Vec<Screen> {
    let main = CGDisplay::main().id;
    CGDisplay::active_displays()
        .unwrap_or_default()
        .into_iter()
        .map(|id| {
            let b = CGDisplay::new(id).bounds();
            Screen {
                x: b.origin.x,
                y: b.origin.y,
                w: b.size.width,
                h: b.size.height,
                scale: 1.0,
                primary: id == main,
            }
        })
        .collect()
}

pub fn buttons_down() -> bool {
    (0..3).any(|button| unsafe { CGEventSourceButtonState(HID_SYSTEM_STATE, button) })
}

pub fn input_blocked() -> bool {
    let raw = unsafe { CGSessionCopyCurrentDictionary() };
    if raw.is_null() {
        return true;
    }
    let session: CFDictionary<CFString, CFType> = unsafe { CFDictionary::wrap_under_create_rule(raw) };
    let flag = |key: &'static str| {
        session
            .find(&CFString::from_static_string(key))
            .and_then(|value| value.downcast::<CFBoolean>())
            .map(bool::from)
    };
    flag("CGSSessionScreenIsLocked").unwrap_or(false) || !flag("kCGSSessionOnConsoleKey").unwrap_or(true)
}

pub fn keys_reachable() -> bool {
    unsafe { IsSecureEventInputEnabled() == 0 }
}

pub fn permitted() -> bool {
    crate::platform::macos::is_process_trusted(false)
        && crate::platform::macos::is_can_input_monitoring(false)
}

pub fn ask_permission() {
    if !crate::platform::macos::is_process_trusted(false) {
        crate::platform::macos::is_process_trusted(true);
        crate::platform::macos::open_privacy_pane("Privacy_Accessibility");
    } else {
        crate::platform::macos::is_can_input_monitoring(true);
    }
}

pub fn lock_file(path: &Path) -> Option<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
        .ok()?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Some(file)
    } else {
        None
    }
}

pub fn recenter(anchor: (f64, f64)) -> bool {
    CGDisplay::warp_mouse_cursor_position(CGPoint::new(anchor.0, anchor.1)).ok();
    CGDisplay::associate_mouse_and_mouse_cursor_position(false).ok();
    true
}

extern "C" fn on_event(_proxy: *mut c_void, kind: u32, event: *mut c_void, _user: *mut c_void) -> *mut c_void {
    if kind == TAP_DISABLED_BY_TIMEOUT || kind == TAP_DISABLED_BY_USER_INPUT {
        let tap = TAP.load(Ordering::SeqCst);
        if tap != 0 {
            unsafe { CGEventTapEnable(tap as CFMachPortRef, true) };
        }
        return event;
    }
    let field = |f: u32| unsafe { CGEventGetIntegerValueField(event, f) };
    let msg = match kind {
        MOVED | LEFT_DRAGGED | RIGHT_DRAGGED | OTHER_DRAGGED if field(SUBTYPE) == TABLET_POINT => {
            let p = unsafe { CGEventGetLocation(event) };
            absolute_move((p.x, p.y))
        }
        MOVED | LEFT_DRAGGED | RIGHT_DRAGGED | OTHER_DRAGGED => relative_move(
            unsafe { CGEventGetDoubleValueField(event, DELTA_X) },
            unsafe { CGEventGetDoubleValueField(event, DELTA_Y) },
        ),
        LEFT_DOWN | LEFT_UP => Some(Msg::Button(MOUSE_BUTTON_LEFT, kind == LEFT_DOWN)),
        RIGHT_DOWN | RIGHT_UP => Some(Msg::Button(MOUSE_BUTTON_RIGHT, kind == RIGHT_DOWN)),
        OTHER_DOWN | OTHER_UP => match field(BUTTON_NUMBER) {
            2 => Some(MOUSE_BUTTON_WHEEL),
            3 => Some(MOUSE_BUTTON_BACK),
            4 => Some(MOUSE_BUTTON_FORWARD),
            _ => None,
        }
        .map(|button| Msg::Button(button, kind == OTHER_DOWN)),
        SCROLL if field(SCROLL_CONTINUOUS) != 0 => Some(Msg::Wheel(
            -field(SCROLL_POINT_X) as i32,
            field(SCROLL_POINT_Y) as i32,
            true,
        )),
        SCROLL => Some(Msg::Wheel(-field(SCROLL_X) as i32, field(SCROLL_Y) as i32, false)),
        _ => None,
    };
    if let Some(msg) = msg {
        post(msg);
    }
    null_mut()
}

fn capture(ready: mpsc::Sender<bool>) {
    let mask = [
        LEFT_DOWN,
        LEFT_UP,
        RIGHT_DOWN,
        RIGHT_UP,
        MOVED,
        LEFT_DRAGGED,
        RIGHT_DRAGGED,
        ROTATE,
        BEGIN_GESTURE,
        END_GESTURE,
        SCROLL,
        OTHER_DOWN,
        OTHER_UP,
        OTHER_DRAGGED,
        GESTURE,
        MAGNIFY,
        SWIPE,
        SMART_MAGNIFY,
        PRESSURE,
    ]
    .iter()
    .fold(0u64, |mask, kind| mask | (1u64 << kind));
    let raw = unsafe {
        CGEventTapCreate(
            HID_EVENT_TAP,
            HEAD_INSERT_EVENT_TAP,
            DEFAULT_TAP_OPTIONS,
            mask,
            on_event,
            null_mut(),
        )
    };
    if raw.is_null() {
        ready.send(false).ok();
        return;
    }
    let port = unsafe { CFMachPort::wrap_under_create_rule(raw) };
    let Ok(source) = port.create_runloop_source(0) else {
        unsafe { CFMachPortInvalidate(raw) };
        ready.send(false).ok();
        return;
    };
    let run_loop = CFRunLoop::get_current();
    run_loop.add_source(&source, unsafe { kCFRunLoopCommonModes });
    unsafe { CGEventTapEnable(raw, true) };
    TAP.store(raw as usize, Ordering::SeqCst);
    RUN_LOOP.store(run_loop.as_concrete_TypeRef() as usize, Ordering::SeqCst);
    ready.send(true).ok();
    while ACTIVE.load(Ordering::SeqCst) {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, Duration::from_millis(250), false);
    }
    TAP.store(0, Ordering::SeqCst);
    unsafe {
        CGEventTapEnable(raw, false);
        CFMachPortInvalidate(raw);
    }
    run_loop.remove_source(&source, unsafe { kCFRunLoopCommonModes });
}

fn hide_cursor() {
    unsafe {
        let default_connection = libc::dlsym(libc::RTLD_DEFAULT, b"_CGSDefaultConnection\0".as_ptr() as _);
        let set_property = libc::dlsym(libc::RTLD_DEFAULT, b"CGSSetConnectionProperty\0".as_ptr() as _);
        if !default_connection.is_null() && !set_property.is_null() {
            let default_connection: extern "C" fn() -> i32 = std::mem::transmute(default_connection);
            let set_property: extern "C" fn(i32, i32, CFStringRef, *const c_void) -> i32 =
                std::mem::transmute(set_property);
            let connection = default_connection();
            let key = CFString::from_static_string("SetsCursorInBackground");
            set_property(
                connection,
                connection,
                key.as_concrete_TypeRef(),
                CFBoolean::true_value().as_CFTypeRef(),
            );
        }
    }
    if CGDisplay::main().hide_cursor().is_ok() {
        HIDDEN.store(true, Ordering::SeqCst);
    }
}

pub fn start_capture(anchor: (f64, f64)) -> Option<(JoinHandle<()>, (f64, f64))> {
    ACTIVE.store(true, Ordering::SeqCst);
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread = std::thread::spawn(move || capture(ready_tx));
    if !ready_rx.recv().unwrap_or(false) {
        ACTIVE.store(false, Ordering::SeqCst);
        thread.join().ok();
        return None;
    }
    CGDisplay::warp_mouse_cursor_position(CGPoint::new(anchor.0, anchor.1)).ok();
    CGDisplay::associate_mouse_and_mouse_cursor_position(false).ok();
    hide_cursor();
    let parked = crate::platform::macos::get_cursor_pos()
        .map_or(anchor, |(x, y)| (x as f64, y as f64));
    Some((thread, parked))
}

pub fn stop_capture(thread: JoinHandle<()>, home: (f64, f64)) {
    ACTIVE.store(false, Ordering::SeqCst);
    let run_loop = RUN_LOOP.swap(0, Ordering::SeqCst);
    if run_loop != 0 {
        unsafe { CFRunLoopStop(run_loop as CFRunLoopRef) };
    }
    thread.join().ok();
    if HIDDEN.swap(false, Ordering::SeqCst) {
        CGDisplay::main().show_cursor().ok();
    }
    CGDisplay::warp_mouse_cursor_position(CGPoint::new(home.0, home.1)).ok();
    CGDisplay::associate_mouse_and_mouse_cursor_position(true).ok();
}
