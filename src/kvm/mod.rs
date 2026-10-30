#![cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]

use crate::{
    client::{Data, Interface},
    input::*,
    ui::{remote::RemoteUiHandler, CUR_SESSION},
    ui_session_interface::Session,
};
use hbb_common::{
    config::Config,
    get_time, log,
    message_proto::{DisplayInfo, Message, TestDelay},
    tokio,
};
use std::{
    cell::Cell,
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering},
        mpsc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

pub mod link;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as platform;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod platform {
    use super::Screen;
    use std::thread::JoinHandle;

    pub const SUPPORTED: bool = false;

    pub fn local_screens() -> Vec<Screen> {
        Vec::new()
    }
    pub fn buttons_down() -> bool {
        false
    }
    pub fn input_blocked() -> bool {
        true
    }
    pub fn keys_reachable() -> bool {
        false
    }
    pub fn permitted() -> bool {
        false
    }
    pub fn ask_permission() {}
    pub fn start_capture(_anchor: (f64, f64)) -> Option<(JoinHandle<()>, (f64, f64))> {
        None
    }
    pub fn stop_capture(_thread: JoinHandle<()>, _home: (f64, f64)) {}
    pub fn recenter(_anchor: (f64, f64)) -> bool {
        false
    }
    pub fn lock_file(_path: &std::path::Path) -> Option<std::fs::File> {
        None
    }
}

const STALL_MS: u64 = 2000;
const TICK: Duration = Duration::from_millis(10);
const PING_EVERY: Duration = Duration::from_millis(500);
const CHECK_EVERY: Duration = Duration::from_millis(100);
const SCREENS_EVERY: Duration = Duration::from_millis(500);
const POLICY_EVERY: Duration = Duration::from_secs(2);
const REARM: Duration = Duration::from_millis(300);
const RETRY: Duration = Duration::from_secs(2);
const MARGIN: f64 = 8.0;
const CORNER: f64 = 100.0;
const WHEEL_MAX: f64 = 3.0;
const ABSOLUTE_GAP: Duration = Duration::from_millis(250);

const CTRL_LEFT: u8 = 1;
const CTRL_RIGHT: u8 = 2;
const ALT_LEFT: u8 = 4;
const ALT_RIGHT: u8 = 8;

const POLICY_UNKNOWN: u8 = 0;
const POLICY_ALLOWED: u8 = 1;
const POLICY_DENIED: u8 = 2;

static ON: AtomicBool = AtomicBool::new(false);
static AWAY: AtomicBool = AtomicBool::new(false);
static LAST_PEER_MS: AtomicU64 = AtomicU64::new(0);
static HOTKEY_MODIFIERS: AtomicU8 = AtomicU8::new(0);
static HOTKEY_ESC_DOWN: AtomicBool = AtomicBool::new(false);
static POLICY: AtomicU8 = AtomicU8::new(POLICY_UNKNOWN);
static RUN: AtomicUsize = AtomicUsize::new(0);

lazy_static::lazy_static! {
    static ref START: Instant = Instant::now();
    static ref TX: Mutex<Option<mpsc::Sender<Msg>>> = Default::default();
    static ref RUNNING: Mutex<Option<Running>> = Default::default();
    static ref REMOTE_DISPLAYS: Mutex<Vec<DisplayInfo>> = Default::default();
}

thread_local! {
    static LAST_ABSOLUTE: Cell<Option<((f64, f64), Instant)>> = const { Cell::new(None) };
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
lazy_static::lazy_static! {
    static ref AWAY_KEYS: Mutex<std::collections::HashSet<rdev::Key>> = Default::default();
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "top" => Some(Self::Top),
            "bottom" => Some(Self::Bottom),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Screen {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub scale: f64,
    pub primary: bool,
}

impl Screen {
    fn contains(&self, p: (f64, f64)) -> bool {
        p.0 >= self.x && p.0 <= self.x + self.w - 1.0 && p.1 >= self.y && p.1 <= self.y + self.h - 1.0
    }
}

pub enum Msg {
    Move(f64, f64),
    Button(i32, bool),
    Wheel(i32, i32, bool),
    RemoteCursor(i32, i32),
    Displays,
    Hotkey,
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub enum KeyRoute {
    Off,
    Local,
    Remote,
    Drop,
}

struct Running {
    thread: JoinHandle<()>,
    _lock: std::fs::File,
}

fn now_ms() -> u64 {
    START.elapsed().as_millis() as u64
}

fn link_ok() -> bool {
    now_ms().saturating_sub(LAST_PEER_MS.load(Ordering::Relaxed)) <= STALL_MS
}

fn cursor_pos() -> Option<(f64, f64)> {
    crate::get_cursor_pos().map(|(x, y)| (x as f64, y as f64))
}

fn post(msg: Msg) {
    if let Some(tx) = TX.lock().unwrap().as_ref() {
        tx.send(msg).ok();
    }
}

fn relative_move(dx: f64, dy: f64) -> Option<Msg> {
    LAST_ABSOLUTE.set(None);
    (dx != 0.0 || dy != 0.0).then_some(Msg::Move(dx, dy))
}

fn absolute_move(p: (f64, f64)) -> Option<Msg> {
    let now = Instant::now();
    match LAST_ABSOLUTE.replace(Some((p, now))) {
        Some((last, at)) if now.duration_since(at) < ABSOLUTE_GAP && last != p => {
            Some(Msg::Move(p.0 - last.0, p.1 - last.1))
        }
        _ => None,
    }
}

fn notify(event: &str) {
    let session = CUR_SESSION.lock().unwrap().clone();
    if let Some(session) = session {
        session.ui_handler.kvm_event(event);
    }
}

fn turn_off(reason: &str) {
    if ON.load(Ordering::SeqCst) {
        log::info!("KVM: {}", reason);
        stop();
        notify("stopped");
    }
}

fn reset_keys() {
    HOTKEY_MODIFIERS.store(0, Ordering::SeqCst);
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    AWAY_KEYS.lock().unwrap().clear();
}

pub fn peer_alive() {
    if ON.load(Ordering::Relaxed) {
        LAST_PEER_MS.store(now_ms(), Ordering::Relaxed);
    }
}

pub fn on_disconnected() {
    turn_off("connection ended");
}

pub fn keyboard_lost() {
    turn_off("keyboard and mouse permission removed");
}

pub fn set_remote_displays(displays: &[DisplayInfo]) {
    *REMOTE_DISPLAYS.lock().unwrap() = displays.to_vec();
    if AWAY.load(Ordering::Relaxed) {
        post(Msg::Displays);
    }
}

pub fn remote_cursor(x: i32, y: i32) {
    if AWAY.load(Ordering::Relaxed) {
        post(Msg::RemoteCursor(x, y));
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn route_key(key: rdev::Key, down: bool) -> KeyRoute {
    if !ON.load(Ordering::Relaxed) {
        return KeyRoute::Off;
    }
    let bit = match key {
        rdev::Key::ControlLeft => CTRL_LEFT,
        rdev::Key::ControlRight => CTRL_RIGHT,
        rdev::Key::Alt => ALT_LEFT,
        rdev::Key::AltGr => ALT_RIGHT,
        _ => 0,
    };
    if bit != 0 {
        if down {
            HOTKEY_MODIFIERS.fetch_or(bit, Ordering::Relaxed);
        } else {
            HOTKEY_MODIFIERS.fetch_and(!bit, Ordering::Relaxed);
        }
    }
    let away = AWAY.load(Ordering::Relaxed);
    if key == rdev::Key::Escape {
        let held = HOTKEY_MODIFIERS.load(Ordering::Relaxed);
        let chord = held & (CTRL_LEFT | CTRL_RIGHT) != 0 && held & (ALT_LEFT | ALT_RIGHT) != 0;
        if down && away && chord {
            HOTKEY_ESC_DOWN.store(true, Ordering::Relaxed);
            post(Msg::Hotkey);
            return KeyRoute::Drop;
        }
        if !down && HOTKEY_ESC_DOWN.swap(false, Ordering::Relaxed) {
            return KeyRoute::Drop;
        }
    }
    let mut keys = AWAY_KEYS.lock().unwrap();
    let remote = if down {
        if away {
            keys.insert(key);
        }
        away || keys.contains(&key)
    } else {
        keys.remove(&key) || away
    };
    if remote {
        KeyRoute::Remote
    } else {
        KeyRoute::Local
    }
}

pub fn available(session: &Session<RemoteUiHandler>) -> bool {
    if !platform::SUPPORTED || !session.is_default() || !*session.server_keyboard_enabled.read().unwrap() {
        return false;
    }
    {
        let lc = session.lc.read().unwrap();
        if lc.direct != Some(true) || lc.get_toggle_option("view-only") {
            return false;
        }
    }
    if Config::get_socks().is_some() {
        return false;
    }
    if !crate::client::direct_peer_addr().map_or(false, |addr| crate::desk::is_private(addr.ip())) {
        return false;
    }
    POLICY.load(Ordering::SeqCst) == POLICY_ALLOWED
}

pub fn prefetch_policy() {
    if !platform::SUPPORTED || POLICY.load(Ordering::SeqCst) != POLICY_UNKNOWN {
        return;
    }
    if !crate::client::direct_peer_addr().map_or(false, |addr| crate::desk::is_private(addr.ip())) {
        return;
    }
    tokio::spawn(async {
        store_policy(!crate::desk::switched_off(&service_options().await.unwrap_or_else(Config::get_options)));
        notify("policy");
    });
}

async fn service_options() -> Option<HashMap<String, String>> {
    let mut conn = crate::ipc::connect(300, "").await.ok()?;
    conn.send(&crate::ipc::Data::Options(None)).await.ok()?;
    match conn.next_timeout(300).await {
        Ok(Some(crate::ipc::Data::Options(Some(options)))) => Some(options),
        _ => None,
    }
}

fn store_policy(allowed: bool) {
    POLICY.store(if allowed { POLICY_ALLOWED } else { POLICY_DENIED }, Ordering::SeqCst);
}

#[tokio::main(flavor = "current_thread")]
async fn read_options() -> HashMap<String, String> {
    service_options().await.unwrap_or_else(Config::get_options)
}

fn read_policy() -> bool {
    let allowed = !crate::desk::switched_off(&read_options());
    store_policy(allowed);
    allowed
}

fn watch_policy(run: usize, peer: String) {
    let linked = !link::side().is_empty();
    let mut missed = 0;
    loop {
        std::thread::sleep(POLICY_EVERY);
        if !ON.load(Ordering::SeqCst) || RUN.load(Ordering::SeqCst) != run {
            return;
        }
        let options = if linked {
            match link::check_service(&mut missed) {
                link::Service::Up(options) => options,
                link::Service::Unsure => continue,
                link::Service::Gone => {
                    turn_off("the service that started this link is gone");
                    return;
                }
            }
        } else {
            read_options()
        };
        if crate::desk::switched_off(&options) {
            store_policy(false);
            turn_off("turned off by policy");
            return;
        }
        if !link::still_wanted(&options, &peer) {
            turn_off(if link::is_remembered() { "this PC was forgotten" } else { "the desk changed" });
            return;
        }
    }
}

pub fn open_settings() {
    platform::ask_permission();
}

fn side_lock(side: &str) -> Option<std::fs::File> {
    Side::parse(side)?;
    platform::lock_file(&Config::path(format!("kvm-{}.lock", side)))
}

pub fn side_free(side: &str) -> bool {
    side_lock(side).is_some()
}

pub fn start(side: &str, display: i32) -> String {
    let Some(edge) = Side::parse(side) else {
        return "unavailable".to_owned();
    };
    let Some(session) = CUR_SESSION.lock().unwrap().clone() else {
        return "unavailable".to_owned();
    };
    if !available(&session) || !read_policy() {
        return "unavailable".to_owned();
    }
    if !platform::permitted() {
        return "permission".to_owned();
    }
    #[cfg(target_os = "macos")]
    crate::keyboard::client::restart_grab_loop_if_stopped();
    stop();
    let Some(lock) = side_lock(side) else {
        return "taken".to_owned();
    };
    let (tx, rx) = mpsc::channel();
    *TX.lock().unwrap() = Some(tx);
    LAST_PEER_MS.store(now_ms(), Ordering::SeqCst);
    reset_keys();
    HOTKEY_ESC_DOWN.store(false, Ordering::SeqCst);
    let run = RUN.fetch_add(1, Ordering::SeqCst) + 1;
    ON.store(true, Ordering::SeqCst);
    session.capture_displays(vec![], vec![], vec![]);
    let peer = session.id.clone();
    let engine = Engine::new(edge, session, display.max(0) as usize);
    let thread = std::thread::spawn(move || engine.run(rx));
    *RUNNING.lock().unwrap() = Some(Running { thread, _lock: lock });
    std::thread::spawn(move || watch_policy(run, peer));
    log::info!("KVM: on, the other computer is on the {}", side);
    String::new()
}

pub fn stop() {
    let running = RUNNING.lock().unwrap().take();
    *TX.lock().unwrap() = None;
    if let Some(running) = running {
        running.thread.join().ok();
        log::info!("KVM: off");
    }
    AWAY.store(false, Ordering::SeqCst);
    ON.store(false, Ordering::SeqCst);
    reset_keys();
}

fn bounds(screens: &[Screen]) -> (f64, f64, f64, f64) {
    screens.iter().fold(
        (f64::MAX, f64::MAX, f64::MIN, f64::MIN),
        |(l, t, r, b), s| (l.min(s.x), t.min(s.y), r.max(s.x + s.w), b.max(s.y + s.h)),
    )
}

fn fraction(v: f64, from: f64, to: f64) -> f64 {
    ((v - from) / (to - from).max(1.0)).clamp(0.0, 1.0)
}

fn at_edge(side: Side, (l, t, r, b): (f64, f64, f64, f64), p: (f64, f64)) -> Option<f64> {
    match side {
        Side::Right if p.0 >= r - 1.0 => Some(fraction(p.1, t, b)),
        Side::Left if p.0 <= l => Some(fraction(p.1, t, b)),
        Side::Top if p.1 <= t => Some(fraction(p.0, l, r)),
        Side::Bottom if p.1 >= b - 1.0 => Some(fraction(p.0, l, r)),
        _ => None,
    }
}

fn crossed_back(side: Side, (l, t, r, b): (f64, f64, f64, f64), p: (f64, f64)) -> Option<f64> {
    match side {
        Side::Right if p.0 < l => Some(fraction(p.1, t, b)),
        Side::Left if p.0 > r - 1.0 => Some(fraction(p.1, t, b)),
        Side::Top if p.1 > b - 1.0 => Some(fraction(p.0, l, r)),
        Side::Bottom if p.1 < t => Some(fraction(p.0, l, r)),
        _ => None,
    }
}

fn near_corner(screens: &[Screen], p: (f64, f64)) -> bool {
    screens.iter().any(|s| {
        let (right, bottom) = (s.x + s.w - 1.0, s.y + s.h - 1.0);
        [(s.x, s.y), (right, s.y), (s.x, bottom), (right, bottom)]
            .iter()
            .any(|c| (c.0 - p.0).abs() < CORNER && (c.1 - p.1).abs() < CORNER)
    })
}

fn remote_entry(side: Side, (l, t, r, b): (f64, f64, f64, f64), f: f64) -> (f64, f64) {
    match side {
        Side::Right => (l, t + f * (b - t)),
        Side::Left => (r - 1.0, t + f * (b - t)),
        Side::Top => (l + f * (r - l), b - 1.0),
        Side::Bottom => (l + f * (r - l), t),
    }
}

fn local_entry(side: Side, (l, t, r, b): (f64, f64, f64, f64), f: f64) -> (f64, f64) {
    match side {
        Side::Right => (r - 1.0, t + f * (b - t)),
        Side::Left => (l, t + f * (b - t)),
        Side::Top => (l + f * (r - l), t),
        Side::Bottom => (l + f * (r - l), b - 1.0),
    }
}

fn clamp(screens: &[Screen], p: (f64, f64), margin: f64) -> (f64, f64) {
    let mut best = p;
    let mut best_distance = f64::MAX;
    for s in screens.iter().filter(|s| s.w >= 2.0 && s.h >= 2.0) {
        let m = margin.min(s.w / 2.0 - 1.0).min(s.h / 2.0 - 1.0).max(0.0);
        let x = p.0.clamp(s.x + m, s.x + s.w - 1.0 - m);
        let y = p.1.clamp(s.y + m, s.y + s.h - 1.0 - m);
        let distance = (x - p.0).powi(2) + (y - p.1).powi(2);
        if distance < best_distance {
            best_distance = distance;
            best = (x, y);
        }
    }
    best
}

fn remote_screens(mac_peer: bool, display: usize) -> Vec<Screen> {
    let displays = REMOTE_DISPLAYS.lock().unwrap();
    let only_current = mac_peer && displays.len() > 1 && displays.iter().any(|d| d.scale > 1.0);
    displays
        .iter()
        .enumerate()
        .filter(|(i, d)| d.width > 0 && d.height > 0 && (!only_current || *i == display))
        .map(|(_, d)| {
            let scale = if d.scale > 0.0 { d.scale } else { 1.0 };
            Screen {
                x: d.x as f64,
                y: d.y as f64,
                w: d.width as f64 / scale,
                h: d.height as f64 / scale,
                scale,
                primary: false,
            }
        })
        .collect()
}

fn to_input(screens: &[Screen], v: (f64, f64)) -> (i32, i32) {
    match screens.iter().find(|s| s.contains(v)).or(screens.first()) {
        Some(s) => (
            (s.x + (v.0 - s.x) * s.scale).round() as i32,
            (s.y + (v.1 - s.y) * s.scale).round() as i32,
        ),
        None => (v.0.round() as i32, v.1.round() as i32),
    }
}

fn from_input(screens: &[Screen], (x, y): (i32, i32)) -> Option<(f64, f64)> {
    let (x, y) = (x as f64, y as f64);
    screens
        .iter()
        .find(|s| x >= s.x && x < s.x + s.w * s.scale && y >= s.y && y < s.y + s.h * s.scale)
        .map(|s| (s.x + (x - s.x) / s.scale, s.y + (y - s.y) / s.scale))
}

fn send_mouse(session: &Session<RemoteUiHandler>, mask: i32, x: i32, y: i32) {
    let (alt, ctrl, shift, command) =
        crate::keyboard::client::get_modifiers_state(false, false, false, false);
    crate::client::send_mouse(mask, x, y, alt, ctrl, shift, command, session);
}

fn ping(session: &Session<RemoteUiHandler>) {
    let mut msg = Message::new();
    msg.set_test_delay(TestDelay {
        time: get_time(),
        from_client: true,
        ..Default::default()
    });
    session.send(Data::Message(msg));
}

struct Away {
    remote: Vec<Screen>,
    v: (f64, f64),
    sent: Option<(i32, i32)>,
    buttons: i32,
    scale: f64,
    exit: (f64, f64),
    anchor: (f64, f64),
    capture: JoinHandle<()>,
    lost: u8,
    swap_buttons: bool,
    reverse_wheel: bool,
    wheel_factor: f64,
    wheel: (f64, f64),
}

struct Engine {
    side: Side,
    session: Session<RemoteUiHandler>,
    mac_peer: bool,
    display: usize,
    screens: Vec<Screen>,
    screens_at: Instant,
    blocked: bool,
    checked_at: Instant,
    pinged_at: Instant,
    rearm_at: Instant,
    ticked: Instant,
    away: Option<Away>,
}

impl Engine {
    fn new(side: Side, session: Session<RemoteUiHandler>, display: usize) -> Self {
        let now = Instant::now();
        let mac_peer = session.peer_platform() == "Mac OS";
        Self {
            side,
            session,
            mac_peer,
            display,
            screens: platform::local_screens(),
            screens_at: now,
            blocked: platform::input_blocked(),
            checked_at: now,
            pinged_at: now,
            rearm_at: now,
            ticked: now,
            away: None,
        }
    }

    fn run(mut self, rx: mpsc::Receiver<Msg>) {
        loop {
            let wait = if self.away.is_none() && self.blocked { CHECK_EVERY } else { TICK };
            match rx.recv_timeout(wait) {
                Ok(msg) => self.handle(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if self.ticked.elapsed() >= TICK {
                self.ticked = Instant::now();
                self.tick();
            }
        }
        self.go_home(None, "stop");
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Move(dx, dy) => self.moved(dx, dy),
            Msg::Button(button, down) => {
                self.flush();
                self.button(button, down);
            }
            Msg::Wheel(dx, dy, pixels) => {
                self.flush();
                self.wheel(dx, dy, pixels);
            }
            Msg::RemoteCursor(x, y) => self.resync(x, y),
            Msg::Displays => self.displays_changed(),
            Msg::Hotkey => self.go_home(None, "hotkey"),
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.pinged_at) >= PING_EVERY {
            self.pinged_at = now;
            ping(&self.session);
        }
        if now.duration_since(self.checked_at) >= CHECK_EVERY {
            self.checked_at = now;
            let blocked = platform::input_blocked();
            if blocked && !self.blocked {
                reset_keys();
            }
            self.blocked = blocked;
        }
        if self.away.is_some() {
            self.flush();
            let reason = self.away_problem();
            if let Some(reason) = reason {
                self.go_home(None, reason);
            }
            return;
        }
        if now < self.rearm_at || self.blocked || !link_ok() {
            return;
        }
        if now.duration_since(self.screens_at) >= SCREENS_EVERY {
            self.screens = platform::local_screens();
            self.screens_at = now;
        }
        if self.screens.is_empty() {
            return;
        }
        let Some(p) = cursor_pos() else {
            return;
        };
        let Some(f) = at_edge(self.side, bounds(&self.screens), p) else {
            return;
        };
        if near_corner(&self.screens, p) || platform::buttons_down() {
            return;
        }
        if !platform::keys_reachable() {
            log::info!("KVM: the front window does not let the keyboard through, mouse stays home");
            self.rearm_at = now + RETRY;
            return;
        }
        self.go_away(p, f);
    }

    fn away_problem(&mut self) -> Option<&'static str> {
        let blocked = self.blocked;
        let away = self.away.as_mut()?;
        let drifted = cursor_pos().map_or(false, |(x, y)| {
            (x - away.anchor.0).abs() > 2.0 || (y - away.anchor.1).abs() > 2.0
        });
        away.lost = if drifted && !platform::recenter(away.anchor) {
            away.lost.saturating_add(1)
        } else {
            0
        };
        if blocked {
            Some("local screen locked or secure desktop")
        } else if !link_ok() {
            log::info!(
                "KVM: no data from the other computer for {} ms",
                now_ms().saturating_sub(LAST_PEER_MS.load(Ordering::Relaxed))
            );
            Some("connection stalled")
        } else if away.lost >= 3 {
            Some("local mouse capture lost")
        } else {
            None
        }
    }

    fn go_away(&mut self, p: (f64, f64), f: f64) {
        let remote = remote_screens(self.mac_peer, self.display);
        if remote.is_empty() {
            return;
        }
        let v = clamp(&remote, remote_entry(self.side, bounds(&remote), f), 0.0);
        let scale = self
            .screens
            .iter()
            .find(|s| s.contains(p))
            .map_or(1.0, |s| s.scale)
            .max(0.25);
        let anchor = self
            .screens
            .iter()
            .find(|s| s.primary)
            .or(self.screens.first())
            .map_or(p, |s| ((s.x + s.w / 2.0).round(), (s.y + s.h / 2.0).round()));
        let Some((capture, anchor)) = platform::start_capture(anchor) else {
            log::error!("KVM: could not capture the local mouse");
            self.rearm_at = Instant::now() + RETRY;
            return;
        };
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        rdev::rehook_keyboard();
        reset_keys();
        let wheel_factor = match self.session.get_option("scroll-speed".to_owned()).as_str() {
            "scroll-speed-2" => 0.5,
            "scroll-speed-4" => 0.25,
            _ => 1.0,
        };
        AWAY.store(true, Ordering::SeqCst);
        self.away = Some(Away {
            remote,
            v,
            sent: None,
            buttons: 0,
            scale,
            exit: p,
            anchor,
            capture,
            lost: 0,
            swap_buttons: self.session.get_toggle_option("swap-left-right-mouse".to_owned()),
            reverse_wheel: self.session.lc.read().unwrap().reverse_mouse_wheel == "Y",
            wheel_factor,
            wheel: (0.0, 0.0),
        });
        self.flush();
        log::info!("KVM: mouse went to the other computer");
    }

    fn go_home(&mut self, f: Option<f64>, reason: &str) {
        let Some(away) = self.away.take() else {
            return;
        };
        AWAY.store(false, Ordering::SeqCst);
        HOTKEY_MODIFIERS.store(0, Ordering::SeqCst);
        self.screens = platform::local_screens();
        self.screens_at = Instant::now();
        let target = match f {
            Some(f) => local_entry(self.side, bounds(&self.screens), f),
            None => away.exit,
        };
        platform::stop_capture(away.capture, clamp(&self.screens, target, MARGIN));
        for button in [
            MOUSE_BUTTON_LEFT,
            MOUSE_BUTTON_RIGHT,
            MOUSE_BUTTON_WHEEL,
            MOUSE_BUTTON_BACK,
            MOUSE_BUTTON_FORWARD,
        ] {
            if away.buttons & button != 0 {
                send_mouse(&self.session, (button << 3) | MOUSE_TYPE_UP, 0, 0);
            }
        }
        crate::keyboard::release_remote_keys_quietly();
        self.rearm_at = Instant::now() + REARM;
        log::info!("KVM: mouse came home ({})", reason);
    }

    fn moved(&mut self, dx: f64, dy: f64) {
        let side = self.side;
        let Some(away) = self.away.as_mut() else {
            return;
        };
        let c = (away.v.0 + dx / away.scale, away.v.1 + dy / away.scale);
        if away.remote.iter().any(|s| s.contains(c)) {
            away.v = c;
            return;
        }
        let inside = clamp(&away.remote, c, 0.0);
        if away.buttons == 0 && !near_corner(&away.remote, inside) {
            if let Some(f) = crossed_back(side, bounds(&away.remote), c) {
                self.go_home(Some(f), "edge");
                return;
            }
        }
        away.v = inside;
    }

    fn flush(&mut self) {
        let Some(away) = self.away.as_mut() else {
            return;
        };
        let p = to_input(&away.remote, away.v);
        if away.sent != Some(p) {
            away.sent = Some(p);
            send_mouse(&self.session, MOUSE_TYPE_MOVE, p.0, p.1);
        }
    }

    fn button(&mut self, button: i32, down: bool) {
        let Some(away) = self.away.as_mut() else {
            return;
        };
        let button = match button {
            MOUSE_BUTTON_LEFT if away.swap_buttons => MOUSE_BUTTON_RIGHT,
            MOUSE_BUTTON_RIGHT if away.swap_buttons => MOUSE_BUTTON_LEFT,
            button => button,
        };
        if down {
            away.buttons |= button;
        } else if away.buttons & button != 0 {
            away.buttons &= !button;
        } else {
            return;
        }
        let kind = if down { MOUSE_TYPE_DOWN } else { MOUSE_TYPE_UP };
        send_mouse(&self.session, (button << 3) | kind, 0, 0);
    }

    fn wheel(&mut self, dx: i32, dy: i32, pixels: bool) {
        let Some(away) = self.away.as_mut() else {
            return;
        };
        let (dx, dy) = if away.reverse_wheel { (-dx, -dy) } else { (dx, dy) };
        if pixels {
            send_mouse(&self.session, MOUSE_TYPE_TRACKPAD, -dx, dy);
            return;
        }
        let step = |v: i32| (v as f64).clamp(-WHEEL_MAX, WHEEL_MAX) * away.wheel_factor;
        away.wheel.0 += step(dx);
        away.wheel.1 += step(dy);
        let (x, y) = (away.wheel.0.trunc(), away.wheel.1.trunc());
        away.wheel.0 -= x;
        away.wheel.1 -= y;
        if x != 0.0 || y != 0.0 {
            send_mouse(&self.session, MOUSE_TYPE_WHEEL, -x as i32, y as i32);
        }
    }

    fn displays_changed(&mut self) {
        let remote = remote_screens(self.mac_peer, self.display);
        let Some(away) = self.away.as_mut() else {
            return;
        };
        if remote.is_empty() {
            return;
        }
        away.v = clamp(&remote, away.v, 0.0);
        away.remote = remote;
        away.sent = None;
    }

    fn resync(&mut self, x: i32, y: i32) {
        let Some(away) = self.away.as_mut() else {
            return;
        };
        if away.buttons != 0 {
            return;
        }
        if let Some(v) = from_input(&away.remote, (x, y)) {
            away.v = v;
            away.sent = Some((x, y));
        }
    }
}
