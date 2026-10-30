use hbb_common::{config::Config, log, tokio, ResultType};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

const CHECK: Duration = Duration::from_secs(2);
const STOP_GRACE: Duration = Duration::from_secs(5);
const HEALTHY: Duration = Duration::from_secs(60);
const FIRST_RETRY: Duration = Duration::from_secs(10);
const LAST_RETRY: Duration = Duration::from_secs(60);
const TOKEN_FRESH: Duration = Duration::from_secs(40);
const REFUSED_RETRY: Duration = Duration::from_secs(300);
const STOPPED: i32 = 3;
const SIGN_IN_FAILED: i32 = 4;
const SIDE_TAKEN: i32 = 5;
const SERVICE_MISSES: u32 = 3;
pub const REMEMBER_OPTION: &str = "kvm-remember";

lazy_static::lazy_static! {
    static ref SIDE: Mutex<String> = Default::default();
    static ref TOKENS: Mutex<HashMap<String, (String, Instant, u32)>> = Default::default();
    static ref LINK_PIDS: Mutex<HashSet<u32>> = Default::default();
}

static REMEMBERED_LINK: AtomicBool = AtomicBool::new(false);
static RUNNER: AtomicBool = AtomicBool::new(false);

pub fn set_side(side: &str, remembered: bool) {
    *SIDE.lock().unwrap() = side.to_owned();
    REMEMBERED_LINK.store(remembered, Ordering::SeqCst);
}

pub fn side() -> String {
    SIDE.lock().unwrap().clone()
}

pub fn is_remembered() -> bool {
    REMEMBERED_LINK.load(Ordering::SeqCst)
}

pub fn exit_code(reason: &str) -> i32 {
    match reason {
        "taken" => SIDE_TAKEN,
        "stopped" if is_remembered() => STOPPED,
        "sign-in" if is_remembered() => SIGN_IN_FAILED,
        _ => 0,
    }
}

fn side_name(side: &str) -> Option<&'static str> {
    ["left", "right", "top", "bottom"].into_iter().find(|s| *s == side)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || "._:-[]".contains(c))
}

fn session_user() -> String {
    #[cfg(windows)]
    return crate::platform::windows::get_current_session_username().unwrap_or_default();
    #[cfg(not(windows))]
    return String::new();
}

pub struct Remembered {
    pub id: String,
    pub side: &'static str,
    user: String,
}

pub fn remembered(value: &str) -> Option<Remembered> {
    let value: serde_json::Value = serde_json::from_str(value).ok()?;
    let id = value["id"].as_str().filter(|id| valid_id(id))?;
    Some(Remembered {
        id: id.to_owned(),
        side: side_name(value["side"].as_str()?)?,
        user: value["user"].as_str().unwrap_or_default().to_owned(),
    })
}

pub fn remember_state(id: &str, side: &str) -> &'static str {
    if !super::platform::SUPPORTED || !valid_id(id) {
        return "";
    }
    let options = crate::ipc::get_options();
    if options.get(crate::desk::OPTION).map_or(false, |layout| !layout.is_empty()) {
        return "";
    }
    match options.get(REMEMBER_OPTION).and_then(|value| remembered(value)) {
        Some(r) if r.id == id && r.side == side => "on",
        _ => "off",
    }
}

pub fn set_remembered(id: &str, side: &str, on: bool) {
    if on {
        if valid_id(id) && side_name(side).is_some() {
            let value = serde_json::json!({ "id": id, "side": side, "user": session_user() });
            crate::ipc::set_option(REMEMBER_OPTION, &value.to_string());
            log::info!("KVM: remembering {} on the {}", id, side);
        }
        return;
    }
    let options = crate::ipc::get_options();
    if options.get(REMEMBER_OPTION).and_then(|value| remembered(value)).map_or(false, |r| r.id == id) {
        crate::ipc::set_option(REMEMBER_OPTION, "");
        log::info!("KVM: forgot {}", id);
    }
}

pub fn still_wanted(options: &HashMap<String, String>, target: &str) -> bool {
    let side = side();
    if side.is_empty() {
        return true;
    }
    let layout = options.get(crate::desk::OPTION).cloned().unwrap_or_default();
    if is_remembered() {
        return layout.is_empty()
            && options
                .get(REMEMBER_OPTION)
                .and_then(|value| remembered(value))
                .map_or(false, |r| r.id == target && r.side == side);
    }
    crate::desk::side_of(&layout, target) == Some(side.as_str())
}

pub fn token_for(target: &str, peer_pid: Option<u32>) -> String {
    let mut tokens = TOKENS.lock().unwrap();
    match tokens.get(target) {
        Some((_, at, pid)) if at.elapsed() < TOKEN_FRESH && Some(*pid) == peer_pid => {
            tokens.remove(target).map(|(token, _, _)| token).unwrap_or_default()
        }
        _ => String::new(),
    }
}

#[tokio::main(flavor = "current_thread")]
pub async fn fetch_token(target: &str) -> String {
    let Ok(mut conn) = crate::ipc::connect(1000, "").await else {
        return String::new();
    };
    for _ in 0..5 {
        if conn.send(&crate::ipc::Data::KvmLinkToken(target.to_owned())).await.is_err() {
            return String::new();
        }
        match conn.next_timeout(3_000).await {
            Ok(Some(crate::ipc::Data::KvmLinkToken(token))) if crate::desk::is_token(&token) => return token,
            Ok(Some(_)) => tokio::time::sleep(Duration::from_millis(200)).await,
            _ => return String::new(),
        }
    }
    String::new()
}

pub fn owns(peer_pid: Option<u32>) -> Option<bool> {
    if !RUNNER.load(Ordering::SeqCst) {
        return None;
    }
    peer_pid.map(|pid| LINK_PIDS.lock().unwrap().contains(&pid))
}

pub enum Service {
    Up(HashMap<String, String>),
    Unsure,
    Gone,
}

#[tokio::main(flavor = "current_thread")]
pub async fn check_service(missed: &mut u32) -> Service {
    let mut conn = match crate::ipc::connect(300, "").await {
        Ok(conn) => conn,
        Err(e) => {
            *missed = if e.downcast_ref::<std::io::Error>().is_some() { *missed + 1 } else { 0 };
            return if *missed >= SERVICE_MISSES { Service::Gone } else { Service::Unsure };
        }
    };
    *missed = 0;
    if conn.send(&crate::ipc::Data::KvmLinkOwned(None)).await.is_err()
        || conn.send(&crate::ipc::Data::Options(None)).await.is_err()
    {
        return Service::Unsure;
    }
    let mut options = None;
    for _ in 0..2 {
        match conn.next_timeout(300).await {
            Ok(Some(crate::ipc::Data::KvmLinkOwned(Some(false)))) => return Service::Gone,
            Ok(Some(crate::ipc::Data::Options(Some(map)))) => options = Some(map),
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    options.map_or(Service::Unsure, Service::Up)
}

#[cfg(windows)]
type Process = crate::platform::windows::UserProcess;

#[cfg(windows)]
fn spawn(args: &[&str]) -> ResultType<Process> {
    crate::platform::windows::spawn_as_user(args)
}

#[cfg(windows)]
fn running(process: &mut Process) -> bool {
    process.running()
}

#[cfg(windows)]
fn ended_with(process: &mut Process) -> Option<i32> {
    process.exit_code().map(|code| code as i32)
}

#[cfg(windows)]
fn kill(process: &mut Process) {
    process.kill();
}

#[cfg(not(windows))]
type Process = std::process::Child;

#[cfg(not(windows))]
fn spawn(args: &[&str]) -> ResultType<Process> {
    Ok(crate::run_me(args.to_vec())?)
}

#[cfg(not(windows))]
fn running(process: &mut Process) -> bool {
    matches!(process.try_wait(), Ok(None))
}

#[cfg(not(windows))]
fn ended_with(process: &mut Process) -> Option<i32> {
    process.try_wait().ok().flatten().and_then(|status| status.code())
}

#[cfg(not(windows))]
fn kill(process: &mut Process) {
    process.kill().ok();
    process.wait().ok();
}

struct Link {
    side: &'static str,
    remembered: bool,
    process: Process,
    started: Instant,
    unwanted_since: Option<Instant>,
}

fn kind(link: &Link) -> &'static str {
    if link.remembered {
        "KVM"
    } else {
        "Desk"
    }
}

pub fn start_runner() {
    if !super::platform::SUPPORTED {
        return;
    }
    #[cfg(target_os = "macos")]
    if crate::platform::is_root() {
        return;
    }
    RUNNER.store(true, Ordering::SeqCst);
    std::thread::spawn(run);
}

fn backoff(retry: &mut HashMap<String, (Instant, Duration)>, target: &str, failed: bool) -> Duration {
    let wait = match retry.get(target) {
        Some((_, last)) if failed => (*last * 2).min(LAST_RETRY),
        _ => FIRST_RETRY,
    };
    retry.insert(target.to_owned(), (Instant::now() + wait, wait));
    wait
}

fn publish(links: &HashMap<String, Link>) {
    *LINK_PIDS.lock().unwrap() = links.values().map(|link| link.process.id()).collect();
}

fn run() {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        return;
    };
    let mut links: HashMap<String, Link> = HashMap::new();
    let mut retry: HashMap<String, (Instant, Duration)> = HashMap::new();
    let mut told_permission = false;
    let mut remember = Config::get_option(REMEMBER_OPTION);
    let mut armed = true;
    loop {
        std::thread::sleep(CHECK);
        let layout = Config::get_option(crate::desk::OPTION);
        let value = Config::get_option(REMEMBER_OPTION);
        if value != remember {
            remember = value;
            armed = false;
        }
        if layout.is_empty() && remember.is_empty() && links.is_empty() {
            retry.clear();
            continue;
        }
        let prelogin = crate::platform::is_prelogin();
        if prelogin {
            armed = true;
        }
        let mut wanted: Vec<(String, &'static str, bool)> = crate::desk::links(&layout, &Config::get_id())
            .into_iter()
            .map(|(target, side)| (target, side, false))
            .collect();
        if layout.is_empty() && armed {
            if let Some(r) = remembered(&remember).filter(|r| r.user == session_user()) {
                wanted.push((r.id, r.side, true));
            }
        }
        if !wanted.is_empty() && (crate::desk::switched_off(&Config::get_options()) || prelogin) {
            wanted.clear();
        }
        if !wanted.is_empty() && !super::platform::permitted() {
            if !told_permission {
                log::info!("Desk: no links until this PC allows Accessibility and Input Monitoring");
                told_permission = true;
            }
            wanted.clear();
        }
        let now = Instant::now();
        let mut forget = false;
        links.retain(|target, link| {
            if running(&mut link.process) {
                return true;
            }
            let lived = link.started.elapsed();
            match ended_with(&mut link.process) {
                Some(STOPPED) if link.remembered => {
                    armed = false;
                    log::info!("KVM: the link to {} was stopped, it comes back at the next sign-in", target);
                }
                Some(SIGN_IN_FAILED) if link.remembered => {
                    armed = false;
                    forget = true;
                    log::info!("KVM: could not sign in to {}, forgetting it", target);
                }
                _ if wanted.iter().any(|(t, _, _)| t == target) => {
                    let wait = backoff(&mut retry, target, lived < HEALTHY);
                    log::info!("{}: link to {} ended after {} s, next try in {} s", kind(link), target, lived.as_secs(), wait.as_secs());
                }
                _ => log::info!("{}: link to {} ended after {} s", kind(link), target, lived.as_secs()),
            }
            false
        });
        if forget {
            Config::set_option(REMEMBER_OPTION.to_owned(), String::new());
            remember.clear();
        }
        if !armed {
            wanted.retain(|(_, _, remembered)| !remembered);
        }
        links.retain(|target, link| {
            if wanted.iter().any(|(t, side, remembered)| t == target && *side == link.side && *remembered == link.remembered) {
                link.unwanted_since = None;
                return true;
            }
            let since = *link.unwanted_since.get_or_insert(now);
            if now.duration_since(since) < STOP_GRACE {
                return true;
            }
            log::info!("{}: stopping the link to {}", kind(link), target);
            kill(&mut link.process);
            false
        });
        publish(&links);
        retry.retain(|target, _| wanted.iter().any(|(t, _, _)| t == target));
        for (target, side, remembered) in wanted {
            if links.contains_key(&target) || retry.get(&target).map_or(false, |(at, _)| now < *at) {
                continue;
            }
            if remembered {
                match spawn(&["--connect", &target, "--kvm-remembered", side]) {
                    Ok(process) => {
                        log::info!("KVM: connecting to {} on the {}, as remembered", target, side);
                        links.insert(target, Link { side, remembered, process, started: Instant::now(), unwanted_since: None });
                        publish(&links);
                    }
                    Err(e) => {
                        log::debug!("KVM: cannot start the link to {}: {}", target, e);
                        backoff(&mut retry, &target, true);
                    }
                }
                continue;
            }
            let token = match rt.block_on(crate::dashboard::desk_link_token(&target)) {
                Ok(token) => token,
                Err(reason) => {
                    if matches!(reason.as_str(), "offline" | "rate" | "network" | "policy") {
                        backoff(&mut retry, &target, true);
                    } else {
                        retry.insert(target.clone(), (Instant::now() + REFUSED_RETRY, LAST_RETRY));
                    }
                    continue;
                }
            };
            match spawn(&["--connect", &target, "--kvm-link", side]) {
                Ok(process) => {
                    TOKENS.lock().unwrap().insert(target.clone(), (token, Instant::now(), process.id()));
                    log::info!("Desk: linking to {} on the {}", target, side);
                    links.insert(target, Link { side, remembered, process, started: Instant::now(), unwanted_since: None });
                    publish(&links);
                }
                Err(e) => {
                    log::debug!("Desk: cannot start the link to {}: {}", target, e);
                    backoff(&mut retry, &target, true);
                }
            }
        }
    }
}
