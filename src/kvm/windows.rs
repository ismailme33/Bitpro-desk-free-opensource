use super::{absolute_move, post, relative_move, Msg, Screen};
use crate::input::*;
use std::{
    cell::{Cell, RefCell},
    fs::File,
    mem,
    os::windows::fs::OpenOptionsExt,
    path::Path,
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering},
        mpsc, OnceLock,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use winapi::{
    shared::{
        minwindef::{BOOL, DWORD, HIWORD, LPARAM, LRESULT, TRUE, UINT, WPARAM},
        windef::{HDC, HMONITOR, HWND, LPRECT},
    },
    um::{
        libloaderapi::{GetModuleHandleW, GetProcAddress, LoadLibraryExA, LOAD_LIBRARY_SEARCH_SYSTEM32},
        processthreadsapi::GetCurrentThreadId,
        winuser::*,
    },
};

pub const SUPPORTED: bool = true;

static ANCHOR_X: AtomicI32 = AtomicI32::new(0);
static ANCHOR_Y: AtomicI32 = AtomicI32::new(0);
static SWAPPED: AtomicBool = AtomicBool::new(false);
static THREAD_ID: AtomicU32 = AtomicU32::new(0);

const RAW_WAIT: Duration = Duration::from_millis(200);
const HID_USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
const HID_USAGE_MOUSE: u16 = 0x02;

enum Pending {
    Move((i32, i32)),
    Ready(Msg),
}

thread_local! {
    static RAW_INPUT: Cell<bool> = const { Cell::new(false) };
    static PENDING: RefCell<Vec<(Instant, Pending)>> = const { RefCell::new(Vec::new()) };
}

type GetDpiForMonitor = unsafe extern "system" fn(HMONITOR, i32, *mut u32, *mut u32) -> i32;

fn monitor_scale(monitor: HMONITOR) -> f64 {
    static FUNC: OnceLock<Option<GetDpiForMonitor>> = OnceLock::new();
    let func = FUNC.get_or_init(|| unsafe {
        let lib = LoadLibraryExA(b"shcore.dll\0".as_ptr() as _, null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
        if lib.is_null() {
            return None;
        }
        let proc = GetProcAddress(lib, b"GetDpiForMonitor\0".as_ptr() as _);
        if proc.is_null() {
            None
        } else {
            Some(mem::transmute::<_, GetDpiForMonitor>(proc))
        }
    });
    let (mut x, mut y) = (0u32, 0u32);
    match func {
        Some(f) if unsafe { f(monitor, 0, &mut x, &mut y) } == 0 && x > 0 => x as f64 / 96.0,
        _ => 1.0,
    }
}

unsafe extern "system" fn add_monitor(monitor: HMONITOR, _: HDC, _: LPRECT, data: LPARAM) -> BOOL {
    let screens = &mut *(data as *mut Vec<Screen>);
    let mut info: MONITORINFO = mem::zeroed();
    info.cbSize = mem::size_of::<MONITORINFO>() as DWORD;
    if GetMonitorInfoW(monitor, &mut info) != 0 {
        let r = info.rcMonitor;
        screens.push(Screen {
            x: r.left as f64,
            y: r.top as f64,
            w: (r.right - r.left) as f64,
            h: (r.bottom - r.top) as f64,
            scale: monitor_scale(monitor),
            primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
    }
    TRUE
}

pub fn local_screens() -> Vec<Screen> {
    let mut screens: Vec<Screen> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            null_mut(),
            null(),
            Some(add_monitor),
            &mut screens as *mut Vec<Screen> as LPARAM,
        );
    }
    screens
}

pub fn buttons_down() -> bool {
    [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2]
        .iter()
        .any(|vk| unsafe { GetAsyncKeyState(*vk) } as u16 & 0x8000 != 0)
}

pub fn input_blocked() -> bool {
    crate::platform::windows::desktop_changed()
}

pub fn keys_reachable() -> bool {
    use crate::platform::windows::{is_elevated, is_foreground_window_elevated};
    !is_foreground_window_elevated().unwrap_or(false) || is_elevated(None).unwrap_or(false)
}

pub fn permitted() -> bool {
    true
}

pub fn ask_permission() {}

pub fn lock_file(path: &Path) -> Option<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .share_mode(0)
        .open(path)
        .ok()
}

pub fn recenter(_anchor: (f64, f64)) -> bool {
    false
}

unsafe extern "system" fn hidden_cursor_proc(hwnd: HWND, msg: UINT, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_SETCURSOR => {
            SetCursor(null_mut());
            TRUE as LRESULT
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_INPUT => {
            raw_input(l as HRAWINPUT);
            DefWindowProcW(hwnd, msg, w, l)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

unsafe fn register_raw_mouse(hwnd: HWND, flags: DWORD) -> bool {
    let device = RAWINPUTDEVICE {
        usUsagePage: HID_USAGE_PAGE_GENERIC_DESKTOP,
        usUsage: HID_USAGE_MOUSE,
        dwFlags: flags,
        hwndTarget: hwnd,
    };
    RegisterRawInputDevices(&device, 1, mem::size_of::<RAWINPUTDEVICE>() as UINT) != 0
}

fn moved_from_anchor(p: (i32, i32)) -> Option<Msg> {
    relative_move(
        (p.0 - ANCHOR_X.load(Ordering::Relaxed)) as f64,
        (p.1 - ANCHOR_Y.load(Ordering::Relaxed)) as f64,
    )
}

fn send(event: Pending, absolute: bool) {
    let msg = match event {
        Pending::Move(p) if absolute => absolute_move((p.0 as f64, p.1 as f64)),
        Pending::Move(p) => moved_from_anchor(p),
        Pending::Ready(msg) => Some(msg),
    };
    if let Some(msg) = msg {
        post(msg);
    }
}

fn release(absolute: bool) {
    for (_, event) in PENDING.take() {
        send(event, absolute);
    }
}

fn hook_event(event: Pending) {
    if RAW_INPUT.get() {
        let (waiting, stale) = PENDING.with_borrow(|pending| {
            (!pending.is_empty(), pending.first().map_or(false, |(at, _)| at.elapsed() >= RAW_WAIT))
        });
        if stale {
            RAW_INPUT.set(false);
            release(false);
        } else if waiting || matches!(event, Pending::Move(_)) {
            PENDING.with_borrow_mut(|pending| pending.push((Instant::now(), event)));
            return;
        }
    }
    send(event, false);
}

unsafe fn raw_input(input: HRAWINPUT) {
    let mut raw: RAWINPUT = mem::zeroed();
    let mut size = mem::size_of::<RAWINPUT>() as UINT;
    let header = mem::size_of::<RAWINPUTHEADER>() as UINT;
    if GetRawInputData(input, RID_INPUT, &mut raw as *mut RAWINPUT as _, &mut size, header) == UINT::MAX
        || raw.header.dwType != RIM_TYPEMOUSE
    {
        return;
    }
    let mouse = raw.data.mouse();
    let absolute = mouse.usFlags & MOUSE_MOVE_ABSOLUTE != 0;
    if absolute || mouse.lLastX != 0 || mouse.lLastY != 0 {
        release(absolute);
    }
}

fn button(left: bool) -> i32 {
    if left != SWAPPED.load(Ordering::Relaxed) {
        MOUSE_BUTTON_LEFT
    } else {
        MOUSE_BUTTON_RIGHT
    }
}

unsafe extern "system" fn mouse_proc(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(null_mut(), code, w, l);
    }
    let info = &*(l as *const MSLLHOOKSTRUCT);
    let data = HIWORD(info.mouseData);
    let extra = || {
        if data == XBUTTON1 {
            MOUSE_BUTTON_BACK
        } else {
            MOUSE_BUTTON_FORWARD
        }
    };
    let msg = match w as UINT {
        WM_MOUSEMOVE => {
            hook_event(Pending::Move((info.pt.x, info.pt.y)));
            None
        }
        WM_LBUTTONDOWN => Some(Msg::Button(button(true), true)),
        WM_LBUTTONUP => Some(Msg::Button(button(true), false)),
        WM_RBUTTONDOWN => Some(Msg::Button(button(false), true)),
        WM_RBUTTONUP => Some(Msg::Button(button(false), false)),
        WM_MBUTTONDOWN => Some(Msg::Button(MOUSE_BUTTON_WHEEL, true)),
        WM_MBUTTONUP => Some(Msg::Button(MOUSE_BUTTON_WHEEL, false)),
        WM_XBUTTONDOWN => Some(Msg::Button(extra(), true)),
        WM_XBUTTONUP => Some(Msg::Button(extra(), false)),
        WM_MOUSEWHEEL => Some(Msg::Wheel(0, data as i16 as i32, false)),
        WM_MOUSEHWHEEL => Some(Msg::Wheel(data as i16 as i32, 0, false)),
        _ => None,
    };
    if let Some(msg) = msg {
        hook_event(Pending::Ready(msg));
    }
    1
}

unsafe fn capture(anchor: (i32, i32), ready: mpsc::Sender<Option<(i32, i32)>>) {
    let module = GetModuleHandleW(null());
    let class = crate::platform::windows::wide_string("HopToDeskKvmCursor");
    let mut wc: WNDCLASSEXW = mem::zeroed();
    wc.cbSize = mem::size_of::<WNDCLASSEXW>() as UINT;
    wc.lpfnWndProc = Some(hidden_cursor_proc);
    wc.hInstance = module;
    wc.lpszClassName = class.as_ptr();
    RegisterClassExW(&wc);
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
        class.as_ptr(),
        class.as_ptr(),
        WS_POPUP,
        anchor.0 - 16,
        anchor.1 - 16,
        32,
        32,
        null_mut(),
        null_mut(),
        module,
        null_mut(),
    );
    if !hwnd.is_null() {
        SetLayeredWindowAttributes(hwnd, 0, 1, LWA_ALPHA);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    let raw_input = !hwnd.is_null() && register_raw_mouse(hwnd, RIDEV_INPUTSINK);
    RAW_INPUT.set(raw_input);
    let start = crate::platform::windows::get_cursor_pos();
    SetCursorPos(anchor.0, anchor.1);
    SetCursor(null_mut());
    let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0);
    if hook.is_null() {
        if let Some((x, y)) = start {
            SetCursorPos(x, y);
        }
        if raw_input {
            register_raw_mouse(null_mut(), RIDEV_REMOVE);
        }
        if !hwnd.is_null() {
            DestroyWindow(hwnd);
        }
        ready.send(None).ok();
        return;
    }
    let (x, y) = crate::platform::windows::get_cursor_pos().unwrap_or(anchor);
    ANCHOR_X.store(x, Ordering::SeqCst);
    ANCHOR_Y.store(y, Ordering::SeqCst);
    THREAD_ID.store(GetCurrentThreadId(), Ordering::SeqCst);
    ready.send(Some((x, y))).ok();
    let mut msg: MSG = mem::zeroed();
    while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    UnhookWindowsHookEx(hook);
    if raw_input {
        register_raw_mouse(null_mut(), RIDEV_REMOVE);
    }
    if !hwnd.is_null() {
        DestroyWindow(hwnd);
    }
}

pub fn start_capture(anchor: (f64, f64)) -> Option<(JoinHandle<()>, (f64, f64))> {
    let anchor = (anchor.0 as i32, anchor.1 as i32);
    ANCHOR_X.store(anchor.0, Ordering::SeqCst);
    ANCHOR_Y.store(anchor.1, Ordering::SeqCst);
    SWAPPED.store(unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0, Ordering::SeqCst);
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread = std::thread::spawn(move || unsafe { capture(anchor, ready_tx) });
    match ready_rx.recv().ok().flatten() {
        Some((x, y)) => Some((thread, (x as f64, y as f64))),
        None => {
            thread.join().ok();
            None
        }
    }
}

pub fn stop_capture(thread: JoinHandle<()>, home: (f64, f64)) {
    let id = THREAD_ID.swap(0, Ordering::SeqCst);
    if id != 0 {
        unsafe {
            PostThreadMessageW(id, WM_QUIT, 0, 0);
        }
    }
    thread.join().ok();
    unsafe {
        SetCursorPos(home.0.round() as i32, home.1.round() as i32);
    }
}
