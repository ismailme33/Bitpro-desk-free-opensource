use hbb_common::{
    allow_err,
    config::{Config, OVERWRITE_SETTINGS},
    log,
    message_proto::RecordingFinished,
    tokio::{self, select, time::Duration},
    ResultType,
};
use sha2::{Sha256, Digest};
use std::sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Mutex};

const DASHBOARD_API_URL: &str = "https://dashboard.hoptodesk.com/api";
const DASHBOARD_WS_URL: &str = "wss://dashboard.hoptodesk.com/socket.io/";
const HEARTBEAT_INTERVAL_SECS: u64 = 120;
const OUTBOX_DRAIN_INTERVAL_SECS: u64 = 10;
const RECONNECT_DELAY_BASE_SECS: u64 = 5;
const RECONNECT_DELAY_MAX_SECS: u64 = 30;
const RECONNECT_HEALTHY_RESET_SECS: u64 = 60;
const WS_LIVENESS_CHECK_SECS: u64 = 30;
const WS_SEND_TIMEOUT_SECS: u64 = 15;
const LINK_RETRY_INTERVAL_SECS: u64 = 300;
const LINK_POLL_INTERVAL_SECS: u64 = 30;
const KEY_SCHEME: u8 = if cfg!(any(target_os = "macos", target_os = "linux", target_os = "freebsd")) { 1 } else { 2 };

static DASHBOARD_RUNNING: AtomicBool = AtomicBool::new(false);
static IN_SESSION: AtomicBool = AtomicBool::new(false);
static SESSION_META: Mutex<(String, String, String, String)> =
    Mutex::new((String::new(), String::new(), String::new(), String::new()));
static TICKET_REPLY_COUNTER: AtomicU64 = AtomicU64::new(0);
static TICKET_STATUS_COUNTER: AtomicU64 = AtomicU64::new(0);

lazy_static::lazy_static! {
    static ref SESSION_NOTIFY: tokio::sync::Notify = tokio::sync::Notify::new();
    static ref NETWORK_APPLY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::new(());
}

const AI_ACTIVITY_IDLE_SECS: u64 = 120;

lazy_static::lazy_static! {
    static ref AI_ACTIVITY: Mutex<Option<(String, std::time::Instant)>> = Mutex::new(None);
}

fn note_ai_activity(data: &serde_json::Value) {
    if data["payload"]["method"].as_str() != Some("tools/call") {
        return;
    }
    let actor: String = data["actor"]
        .as_str()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control() && *c != '{' && *c != '}')
        .take(60)
        .collect();
    if actor.trim().is_empty() {
        return;
    }
    *AI_ACTIVITY.lock().unwrap() = Some((actor.trim().to_owned(), std::time::Instant::now()));
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    crate::server::video_service::note_ai_activity(actor.trim());
}

pub fn get_ai_activity() -> String {
    match AI_ACTIVITY.lock().unwrap().as_ref() {
        Some((actor, at)) if at.elapsed().as_secs() < AI_ACTIVITY_IDLE_SECS => actor.clone(),
        _ => String::new(),
    }
}

type NetworkSettingsEvent = (String, serde_json::Value);
lazy_static::lazy_static! {
    static ref NETWORK_APPLY_TX: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<NetworkSettingsEvent>>> =
        std::sync::Mutex::new(None);
}

fn start_network_apply_worker() {
    let mut slot = NETWORK_APPLY_TX.lock().unwrap();
    if slot.as_ref().map_or(false, |tx| !tx.is_closed()) {
        return;
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<NetworkSettingsEvent>();
    *slot = Some(tx);
    tokio::spawn(async move {
        while let Some((network_type, api_json)) = rx.recv().await {
            if let Err(e) = apply_network_settings(&network_type, &api_json).await {
                log::warn!("Failed to apply network settings: {}", e);
            }
        }
    });
}

fn queue_network_settings(event: NetworkSettingsEvent) {
    let slot = NETWORK_APPLY_TX.lock().unwrap();
    match slot.as_ref() {
        Some(tx) if tx.send(event).is_ok() => {}
        _ => log::warn!("Network settings apply worker is not running"),
    }
}

pub fn is_linked() -> bool {
    !Config::get_option("dashboard_user_id").is_empty()
}

pub fn set_pending_quick_connect_token(token: &str) {
    Config::set_option("pending_quick_connect_token".to_owned(), token.to_owned());
}

pub fn has_pending_quick_connect_token() -> bool {
    !Config::get_option("pending_quick_connect_token").is_empty()
}

pub fn take_pending_quick_connect_token() -> String {
    let token = Config::get_option("pending_quick_connect_token");
    if !token.is_empty() {
        Config::set_option("pending_quick_connect_token".to_owned(), String::new());
    }
    token
}

pub fn get_invite_code() -> String {
    let code = Config::get_option("invite_code");
    if !code.is_empty() {
        return code;
    }
    if let Ok(code) = std::fs::read_to_string(Config::path("InviteCode.toml")) {
        let code = code.trim().to_string();
        if !code.is_empty() {
            return code;
        }
    }
    if let Ok(code) = std::fs::read_to_string(Config::shared_path("InviteCode.toml")) {
        let code = code.trim().to_string();
        if !code.is_empty() {
            return code;
        }
    }
    String::new()
}

pub fn get_dashboard_user_id() -> String {
    Config::get_option("dashboard_user_id")
}

fn dashboard_ws_url() -> String {
    let v = Config::get_option("dashboard-ws-url");
    if v.is_empty() { DASHBOARD_WS_URL.to_string() } else { v }
}

fn dashboard_api_base() -> String {
    let v = Config::get_option("dashboard-api-url");
    if v.is_empty() { DASHBOARD_API_URL.to_string() } else { v }
}

pub fn dashboard_origin() -> String {
    dashboard_api_base()
        .trim_end_matches('/')
        .trim_end_matches("/api")
        .to_string()
}

pub fn portal_url(slug: &str) -> String {
    if slug.is_empty() {
        String::new()
    } else {
        format!("{}/portal/{}", web_base_url().trim_end_matches('/'), slug)
    }
}

pub fn web_base_url() -> String {
    dashboard_api_base()
        .trim_end_matches('/')
        .trim_end_matches("/api")
        .to_string()
}

fn apply_dashboard_endpoint(key: &str, field: &serde_json::Value, scheme: &str, path: &str) {
    if Config::get_option("dashboard-region-pinned") == "Y" {
        return;
    }
    let desired = match (field["host"].as_str(), field["port"].as_str()) {
        (Some(host), Some(port)) => format!("{}://{}:{}{}", scheme, host, port, path),
        _ => String::new(),
    };
    if Config::get_option(key) != desired {
        Config::set_option(key.to_owned(), desired);
    }
}

async fn refresh_dashboard_endpoints() {
    let map = match hbb_common::api::call_api().await {
        Ok(m) => m,
        Err(_) => return,
    };
    apply_dashboard_endpoint("dashboard-ws-url", &map["dashboardws"], "wss", "/socket.io/");
    apply_dashboard_endpoint("dashboard-api-url", &map["dashboardapi"], "https", "/api");
}

fn apply_region_from_response(resp: &serde_json::Value) {
    let region = &resp["dashboard_region"];
    if let (Some(api), Some(ws)) = (region["api"].as_str(), region["ws"].as_str()) {
        if api.starts_with("https://") && ws.starts_with("wss://") {
            Config::set_option("dashboard-api-url".to_owned(), api.to_owned());
            Config::set_option("dashboard-ws-url".to_owned(), ws.to_owned());
            Config::set_option("dashboard-region-pinned".to_owned(), "Y".to_owned());
        }
    }
}

pub async fn validate_invite(invite_code: &str) -> ResultType<(String, String, String, String)> {
    refresh_dashboard_endpoints().await;
    let client = crate::common::make_http_client();
    let url = format!(
        "{}?action=validateInvite&invite_code={}",
        dashboard_api_base(), invite_code
    );
    let resp: serde_json::Value = client.get(&url).send().await?.json().await?;
    if resp["success"].as_bool() != Some(true) {
        hbb_common::bail!("validateInvite failed: {}", resp);
    }
    apply_region_from_response(&resp);
    let invite = &resp["invite"];
    let enrollment_token = invite["enrollment_token"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let dashboard_user_id = invite["dashboard_user_id"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let invite_type = invite["invite_type"].as_str().unwrap_or("standard").to_string();
    let account_name = invite["account_name"]
        .as_str()
        .unwrap_or("")
        .to_string();
    if dashboard_user_id.is_empty() || dashboard_user_id.starts_with("DASH-") {
        hbb_common::bail!("Invalid dashboard_user_id: {}", dashboard_user_id);
    }
    Ok((enrollment_token, dashboard_user_id, invite_type, account_name))
}

pub async fn get_share_invite() -> ResultType<String> {
    refresh_dashboard_endpoints().await;
    let dashboard_user_id = get_dashboard_user_id();
    if dashboard_user_id.is_empty() {
        hbb_common::bail!("Not linked to a dashboard");
    }
    let device_id = Config::get_id();
    if device_id.is_empty() {
        hbb_common::bail!("Device ID not available");
    }
    let client = crate::common::make_http_client();
    let url = format!("{}?action=getShareInvite", dashboard_api_base());
    let device_key = device_key(follows_service()).await;
    let mut params: Vec<(&str, &str)> = vec![
        ("device_id", &device_id),
        ("dashboard_user_id", &dashboard_user_id),
    ];
    if !device_key.is_empty() {
        params.push(("device_key", &device_key));
    }
    let resp: serde_json::Value = client.post(&url).form(&params).send().await?.json().await?;
    if resp["success"].as_bool() != Some(true) {
        hbb_common::bail!("getShareInvite failed: {}", resp);
    }
    let code = resp["invite_code"].as_str().unwrap_or("").to_string();
    if code.is_empty() {
        hbb_common::bail!("getShareInvite returned empty code");
    }
    Ok(code)
}

pub fn get_device_key() -> String {
    Config::get_option("dashboard-device-key")
}

fn follows_service() -> bool {
    #[cfg(windows)]
    return !crate::is_server()
        && !crate::common::is_server_running()
        && crate::platform::is_installed()
        && crate::platform::is_self_service_running();
    #[cfg(not(windows))]
    false
}

fn leaves_remote_commands_to_server() -> bool {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return !crate::is_server()
        && !crate::common::is_server_running()
        && crate::platform::is_installed();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    follows_service()
}

#[cfg(windows)]
const SERVICE_LINK_OPTIONS: [&str; 8] = [
    "dashboard_user_id",
    "dashboard_device_id",
    "dashboard_account_name",
    "dashboard-device-key",
    "dashboard-api-url",
    "dashboard-ws-url",
    "dashboard-region-pinned",
    "last_enrolled_invite_code",
];

#[cfg(windows)]
async fn mirror_service_link() {
    let Ok(mut conn) = crate::ipc::connect(1000, "").await else {
        return;
    };
    if conn.send(&crate::ipc::Data::Options(None)).await.is_err() {
        return;
    }
    let Ok(Some(crate::ipc::Data::Options(Some(options)))) = conn.next_timeout(1000).await else {
        return;
    };
    for key in SERVICE_LINK_OPTIONS {
        Config::set_option(key.to_owned(), options.get(key).cloned().unwrap_or_default());
    }
}

#[cfg(windows)]
pub fn keep_service_owned_options(
    incoming: &mut std::collections::HashMap<String, String>,
    current: &std::collections::HashMap<String, String>,
) {
    let locked = current.get("dashboard_user_id").map_or(false, |v| !v.is_empty())
        && current.get("account-access-only").map_or(false, |v| v == "Y");
    let keys: Vec<String> = incoming.keys().chain(current.keys()).cloned().collect();
    for key in keys {
        let owned = key.starts_with("policy-")
            || matches!(
                key.as_str(),
                "account-access-only"
                    | "enable-easy-access"
                    | "session-notes-enabled"
                    | crate::desk::OPTION
                    | "tamper-code-hash"
                    | "tamper-code-salt"
            )
            || (locked && (key == "invite_code" || SERVICE_LINK_OPTIONS.contains(&key.as_str())));
        if !owned || incoming.get(&key) == current.get(&key) {
            continue;
        }
        log::warn!("IPC: kept the service value of {}", key);
        match current.get(&key) {
            Some(value) => incoming.insert(key, value.clone()),
            None => incoming.remove(&key),
        };
    }
}

#[cfg(windows)]
#[tokio::main(flavor = "current_thread")]
pub async fn load_service_link() {
    if follows_service() {
        mirror_service_link().await;
    }
}

async fn device_key(follower: bool) -> String {
    #[cfg(windows)]
    if follower {
        mirror_service_link().await;
    }
    let _ = follower;
    get_device_key()
}

const LINK_LOCK_STALE_SECS: u64 = 120;
const LINK_LOCK_WAIT_SECS: f32 = 5.0;

struct LinkLock(Option<(std::path::PathBuf, String)>);

impl Drop for LinkLock {
    fn drop(&mut self) {
        if let Some((path, token)) = &self.0 {
            if std::fs::read_to_string(path).ok().as_deref() == Some(token.as_str()) {
                std::fs::remove_file(path).ok();
            }
        }
    }
}

fn create_link_lock(path: &std::path::Path) -> std::io::Result<LinkLock> {
    use std::io::Write;
    let token = format!("{}-{}", std::process::id(), hbb_common::rand::random::<u64>());
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(token.as_bytes())?;
    Ok(LinkLock(Some((path.to_path_buf(), token))))
}

fn acquire_link_lock() -> Option<LinkLock> {
    use std::io::ErrorKind::AlreadyExists;
    let path = Config::shared_path("Linking.lock");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    match create_link_lock(&path) {
        Ok(lock) => return Some(lock),
        Err(e) if e.kind() != AlreadyExists => return Some(LinkLock(None)),
        Err(_) => {}
    }
    let stale = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map_or(true, |age| age.as_secs() >= LINK_LOCK_STALE_SECS))
        .unwrap_or(true);
    if !stale {
        return None;
    }
    if std::fs::remove_file(&path).is_err() && path.exists() {
        return Some(LinkLock(None));
    }
    match create_link_lock(&path) {
        Ok(lock) => Some(lock),
        Err(e) if e.kind() == AlreadyExists => None,
        Err(_) => Some(LinkLock(None)),
    }
}

fn store_device_key(value: &serde_json::Value) -> bool {
    if let Some(key) = value.as_str() {
        if !key.is_empty() && key != get_device_key() {
            Config::set_option("dashboard-device-key".to_owned(), key.to_string());
            log::info!("Dashboard: stored device key");
            return true;
        }
    }
    false
}

pub const DEVICE_KEY_ERROR_CODES: [&str; 2] = ["device_key_invalid", "device_key_required"];

fn forget_dashboard_link(reason: &str) {
    log::warn!("Dashboard: {}, unlinking", reason);
    Config::set_option("dashboard_user_id".to_owned(), String::new());
    Config::set_option("dashboard_device_name".to_owned(), String::new());
    Config::set_option("dashboard-device-key".to_owned(), String::new());
    Config::set_option("invite_code".to_owned(), String::new());
    remove_enrollment_artifacts();
}

fn apply_access_lock(locked: bool) {
    let value = if locked { "Y" } else { "" };
    if Config::get_option("account-access-only") != value {
        Config::set_option("account-access-only".to_owned(), value.to_owned());
        log::info!("Dashboard: account access only set to {}", locked);
    }
    if locked {
        set_policy_option("enable-easy-access", "Y");
    }
}

fn apply_easy_access(enabled: bool) {
    let on = enabled || Config::get_option("account-access-only") == "Y";
    set_policy_option("enable-easy-access", if on { "Y" } else { "" });
}

fn apply_session_notes(enabled: bool) {
    let value = if enabled { "Y" } else { "" };
    if Config::get_option("session-notes-enabled") != value {
        Config::set_option("session-notes-enabled".to_owned(), value.to_owned());
        log::info!("Dashboard: session notes set to {}", enabled);
    }
}

const POLICY_OPTIONS: [(&str, &str); 8] = [
    ("file_transfer", "enable-file-transfer"),
    ("clipboard", "enable-clipboard"),
    ("keyboard", "enable-keyboard"),
    ("audio", "enable-audio"),
    ("remote_printing", "enable-remote-printing"),
    ("terminal", "enable-terminal"),
    ("tunnel", "enable-tunnel"),
    ("kvm", "enable-kvm"),
];

pub fn policy_option_key(enable_prefix_option: &str) -> String {
    format!("policy-{}", enable_prefix_option)
}

pub const POLICY_FORCED_RECORDING: &str = "policy-force-record-incoming";
pub const POLICY_SESSION_TIMEOUT: &str = "policy-session-timeout";
pub const POLICY_REQUIRE_APPROVAL: &str = "policy-require-approval";
pub const POLICY_RECORDING_RETENTION: &str = "policy-recording-retention-days";

pub fn log_file_transfer(transfer_type: &'static str, path: &str, files: Vec<(String, i64)>, peer_name: String) {
    let duid = get_dashboard_user_id();
    let device_id = Config::get_id();
    if duid.is_empty() || device_id.is_empty() || files.is_empty() {
        return;
    }
    let base = dashboard_api_base();
    let log_names = Config::get_option("log-file-names") == "Y";
    let path = if log_names { path.to_owned() } else { String::new() };
    let log_token = Config::get_option("dashboard-log-token");
    tokio::spawn(async move {
        let client = crate::common::make_http_client();
        for (name, size) in files.into_iter().take(20) {
            let file_name = if !log_names { String::new() } else if name.is_empty() { path.clone() } else { name };
            if log_names && file_name.is_empty() {
                continue;
            }
            let body = serde_json::json!({
                "action": "logFileTransfer",
                "dashboard_user_id": duid,
                "device_id": device_id,
                "user_name": peer_name,
                "transfer_type": transfer_type,
                "file_name": file_name,
                "file_path": path,
                "log_file_names": log_names,
                "file_size": size,
                "transfer_status": "started",
                "log_token": log_token,
            });
            if let Err(e) = client.post(&base).json(&body).send().await {
                log::debug!("Dashboard: file transfer log failed: {}", e);
                break;
            }
        }
    });
}

pub fn policy_forces_recording() -> bool {
    is_linked() && Config::get_option(POLICY_FORCED_RECORDING) == "Y"
}

pub fn policy_requires_approval() -> bool {
    is_linked()
        && Config::get_option(POLICY_REQUIRE_APPROVAL) == "Y"
        && !OVERWRITE_SETTINGS.read().unwrap().contains_key("approve-mode")
}

pub fn is_option_forced(key: &str) -> bool {
    if !is_linked() {
        return false;
    }
    match key {
        "enable-easy-access" => true,
        "allow-auto-record-incoming" => Config::get_option(POLICY_FORCED_RECORDING) == "Y",
        "recording-retention-days" => !Config::get_option(POLICY_RECORDING_RETENTION).is_empty(),
        "approve-mode" => Config::get_option(POLICY_REQUIRE_APPROVAL) == "Y",
        "allow-auto-disconnect" | "auto-disconnect-timeout" => {
            !Config::get_option(POLICY_SESSION_TIMEOUT).is_empty()
        }
        _ => {
            POLICY_OPTIONS.iter().any(|(_, option)| *option == key)
                && Config::get_option(&policy_option_key(key)) == "N"
        }
    }
}

pub fn policy_session_timeout() -> Option<u64> {
    if !is_linked() {
        return None;
    }
    match Config::get_option(POLICY_SESSION_TIMEOUT).parse::<u64>() {
        Ok(minutes) if minutes > 0 => Some(minutes),
        _ => None,
    }
}

fn set_policy_option(key: &str, value: &str) {
    if Config::get_option(key) != value {
        Config::set_option(key.to_owned(), value.to_owned());
        log::info!("Dashboard: device policy {} set to {}", key, value);
    }
}

fn apply_device_policy(data: &serde_json::Value) {
    for (field, option) in POLICY_OPTIONS.iter() {
        let denied = data[*field].as_bool() == Some(false);
        let value = if denied { "N" } else { "" };
        let key = policy_option_key(option);
        if Config::get_option(&key) != value {
            Config::set_option(key, value.to_owned());
            log::info!(
                "Dashboard: device policy {} {}",
                option,
                if denied { "blocked" } else { "released" }
            );
        }
    }

    let was_forced_recording = Config::get_option(POLICY_FORCED_RECORDING) == "Y";
    let force_recording = data["recording"].as_bool() == Some(true);
    set_policy_option(POLICY_FORCED_RECORDING, if force_recording { "Y" } else { "" });
    if force_recording {
        force_user_option("allow-auto-record-incoming", "Y", was_forced_recording);
    } else if was_forced_recording {
        restore_user_option("allow-auto-record-incoming");
    }

    let was_requiring_approval = Config::get_option(POLICY_REQUIRE_APPROVAL) == "Y";
    let require_approval = data["require_approval"].as_bool() == Some(true);
    set_policy_option(POLICY_REQUIRE_APPROVAL, if require_approval { "Y" } else { "" });
    if require_approval {
        force_user_option("approve-mode", "click", was_requiring_approval);
    } else if was_requiring_approval {
        restore_user_option("approve-mode");
    }

    let had_retention = !Config::get_option(POLICY_RECORDING_RETENTION).is_empty();
    let retention = data["recording_retention_days"].as_u64().unwrap_or(0);
    set_policy_option(
        POLICY_RECORDING_RETENTION,
        &if retention > 0 { retention.to_string() } else { String::new() },
    );
    if retention > 0 {
        force_user_option("recording-retention-days", &retention.to_string(), had_retention);
    } else if had_retention {
        restore_user_option("recording-retention-days");
    }

    let had_timeout = !Config::get_option(POLICY_SESSION_TIMEOUT).is_empty();
    let timeout = data["session_timeout"].as_u64().unwrap_or(0);
    set_policy_option(
        POLICY_SESSION_TIMEOUT,
        &if timeout > 0 { timeout.to_string() } else { String::new() },
    );
    if timeout > 0 {
        force_user_option("allow-auto-disconnect", "Y", had_timeout);
        force_user_option("auto-disconnect-timeout", &timeout.to_string(), had_timeout);
    } else if had_timeout {
        restore_user_option("allow-auto-disconnect");
        restore_user_option("auto-disconnect-timeout");
    }
}

fn release_device_policy() {
    apply_device_policy(&serde_json::Value::Null);
}

fn saved_option_key(option: &str) -> String {
    format!("policy-saved-{}", option)
}

fn force_user_option(option: &str, value: &str, was_forced: bool) {
    if !was_forced {
        Config::set_option(saved_option_key(option), Config::get_option(option));
    }
    set_policy_option(option, value);
}

fn restore_user_option(option: &str) {
    let saved_key = saved_option_key(option);
    set_policy_option(option, &Config::get_option(&saved_key));
    Config::set_option(saved_key, String::new());
}

pub async fn verify_easy_access(token: &str) -> Option<String> {
    let device_id = Config::get_id();
    if device_id.is_empty() {
        return None;
    }
    let client = crate::common::make_http_client();
    let url = format!("{}?action=verifyEasyAccess", dashboard_api_base());
    let body = serde_json::json!({ "token": token, "device_id": device_id });
    let resp = match client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            log::warn!("verifyEasyAccess request failed: {}", e);
            return None;
        }
    };
    let value: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            log::warn!("verifyEasyAccess bad response: {}", e);
            return None;
        }
    };
    if value["ok"].as_bool() != Some(true) {
        return None;
    }
    let authorized_by = value["authorized_by"]["email"]
        .as_str()
        .or_else(|| value["authorized_by"]["name"].as_str())
        .unwrap_or("dashboard user")
        .to_string();
    Some(authorized_by)
}

async fn device_call(action: &str, extra: serde_json::Value) -> Option<serde_json::Value> {
    let (device_id, duid, key) = (Config::get_id(), get_dashboard_user_id(), get_device_key());
    if device_id.is_empty() || duid.is_empty() || key.is_empty() {
        return None;
    }
    let mut body = serde_json::json!({ "device_id": device_id, "dashboard_user_id": duid, "device_key": key });
    if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        body.extend(extra.clone());
    }
    let url = format!("{}?action={}", dashboard_api_base(), action);
    let resp = crate::common::make_http_client()
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| log::warn!("{} request failed: {}", action, e))
        .ok()?;
    resp.json().await.map_err(|e| log::warn!("{} bad response: {}", action, e)).ok()
}

pub async fn desk_link_token(target: &str) -> Result<String, String> {
    let Some(value) = device_call("deskLinkToken", serde_json::json!({ "target": target })).await else {
        return Err("network".to_owned());
    };
    match value["token"].as_str() {
        Some(token) if value["ok"].as_bool() == Some(true) && crate::desk::is_token(token) => Ok(token.to_owned()),
        _ => {
            let reason = value["reason"].as_str().unwrap_or("unknown").to_owned();
            log::info!("Desk: no link token for {}: {}", target, reason);
            Err(reason)
        }
    }
}

pub async fn desk_link_verify(token: &str, peer: &str) -> bool {
    let Some(value) = device_call("deskLinkVerify", serde_json::json!({ "token": token, "peer": peer })).await else {
        return false;
    };
    if value["ok"].as_bool() != Some(true) {
        log::info!("Desk: the dashboard refused the link from {}: {}", peer, value["reason"].as_str().unwrap_or("unknown"));
        return false;
    }
    crate::desk::store(&value["desk"]);
    true
}

pub fn desk_link_ended(peer: String, seconds: u64) {
    hbb_common::tokio::spawn(async move {
        device_call("deskLinkEnded", serde_json::json!({ "peer": peer, "seconds": seconds })).await;
    });
}

pub const DEVICE_LIMIT_ERROR_PREFIX: &str = "device_limit";
pub fn upgrade_url() -> String {
    format!("{}/pro#billing", web_base_url())
}

pub fn is_device_limit_error(e: &hbb_common::anyhow::Error) -> bool {
    e.to_string().starts_with(DEVICE_LIMIT_ERROR_PREFIX)
}

pub fn take_link_error() -> String {
    let msg = Config::get_option("dashboard_link_error");
    if !msg.is_empty() {
        Config::set_option("dashboard_link_error".to_owned(), String::new());
    }
    msg
}

fn set_link_error(e: &hbb_common::anyhow::Error) {
    let msg = e.to_string();
    let display = msg
        .strip_prefix(&format!("{}: ", DEVICE_LIMIT_ERROR_PREFIX))
        .unwrap_or(&msg)
        .to_string();
    Config::set_option("dashboard_link_error".to_owned(), display);
}

pub fn check_invite_code(invite_code: &str) -> ResultType<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let url = format!(
        "{}?action=validateInvite&invite_code={}",
        dashboard_api_base(), invite_code
    );
    let resp: serde_json::Value = client.get(&url).send()?.json()?;
    if resp["success"].as_bool() != Some(true) {
        let reason = resp["message"]
            .as_str()
            .or_else(|| resp["error"].as_str())
            .unwrap_or("")
            .to_string();
        hbb_common::bail!("{}", reason);
    }
    apply_region_from_response(&resp);
    Ok(())
}

pub async fn register_device(
    enrollment_token: &str,
    invite_code: &str,
    device_id: &str,
    device_name: &str,
    os_name: &str,
    mac: &str,
) -> ResultType<String> {
    let client = crate::common::make_http_client();
    let url = format!("{}?action=registerDevice", dashboard_api_base());
    let mut params: Vec<(&str, &str)> = vec![
        ("device_id", device_id),
        ("device_name", device_name),
        ("computer_name", device_name),
        ("os", os_name),
        ("mac_address", mac),
    ];
    if !enrollment_token.is_empty() {
        params.push(("enrollment_token", enrollment_token));
    }
    if !invite_code.is_empty() {
        params.push(("invite_code", invite_code));
    }
    params.push(("key_capable", "1"));
    let device_key = get_device_key();
    if !device_key.is_empty() {
        params.push(("device_key", &device_key));
    }
    let build_id = hbb_common::config::custom_build_id();
    if !build_id.is_empty() {
        params.push(("build_id", &build_id));
    }
    let resp: serde_json::Value = client.post(&url).form(&params).send().await?.json().await?;
    apply_region_from_response(&resp);
    if resp["success"].as_bool() != Some(true) {
        let reason = resp["error"].as_str().unwrap_or("registerDevice failed").to_string();
        if resp["code"].as_str() == Some("device_limit") {
            hbb_common::bail!("{}: {}", DEVICE_LIMIT_ERROR_PREFIX, reason);
        }
        hbb_common::bail!("registerDevice failed: {}", reason);
    }
    store_device_key(&resp["device_key"]);
    let dashboard_user_id = resp["dashboard_user_id"]
        .as_str()
        .unwrap_or("")
        .to_string();
    log::info!("Device registered with dashboard successfully (user_id={})", dashboard_user_id);

    if let Some(account_name) = resp["account_name"].as_str() {
        if !account_name.is_empty() {
            Config::set_option("dashboard_account_name".to_owned(), account_name.to_owned());
        }
    }

    // Apply deployment profile settings if provided
    if let Some(settings) = resp.get("deployment_settings") {
        if settings["enable_unattended_access"].as_bool() == Some(true) {
            Config::set_option("unattended-access".to_owned(), "true".to_owned());
            log::info!("Deployment profile: enabled unattended access");
        }
        if let Some(password) = settings["default_password"].as_str() {
            if !password.is_empty() {
                Config::set_permanent_password(password);
                log::info!("Deployment profile: set default password");
            }
        }
        if settings["enable_auto_record"].as_bool() == Some(true) {
            Config::set_option("allow-auto-record-incoming".to_owned(), "Y".to_owned());
            log::info!("Deployment profile: enabled automatic session recording");
        }
        if settings["enable_easy_access"].as_bool() == Some(true) {
            Config::set_option("enable-easy-access".to_owned(), "Y".to_owned());
            log::info!("Deployment profile: enabled Easy Access");
        }
        if let Some(code) = settings["tamper_code"].as_str() {
            if !code.is_empty() && Config::get_option("tamper-code-hash").is_empty() {
                let salt = Config::get_auto_password(16);
                Config::set_option("tamper-code-hash".to_owned(), tamper_code_hash(&salt, code));
                Config::set_option("tamper-code-salt".to_owned(), salt);
                log::info!("Deployment profile: stored device uninstall code");
            }
        }
    }

    if let Some(network) = resp.get("network") {
        if let Some((network_type, api_json)) = network_settings_from_event(network) {
            if let Err(e) = apply_network_settings(&network_type, &api_json).await {
                log::warn!("Failed to apply network settings from registration: {}", e);
            }
        }
    }

    Ok(dashboard_user_id)
}

pub async fn get_network_settings(invite_code: &str) -> ResultType<()> {
    let client = crate::common::make_http_client();
    let device_key = device_key(follows_service()).await;
    let keyed = if device_key.is_empty() {
        String::new()
    } else {
        format!("&device_id={}&device_key={}", Config::get_id(), device_key)
    };
    let url = format!(
        "{}?action=getNetworkSettingsByInvite&invite_code={}{}",
        dashboard_api_base(), invite_code, keyed
    );
    let resp: serde_json::Value = client.get(&url).send().await?.json().await?;
    if resp["success"].as_bool() != Some(true) {
        log::warn!("getNetworkSettingsByInvite: not successful or not available");
        return Ok(());
    }
    let network_type = resp["network_type"].as_str().unwrap_or("hoptodesk");
    let api_json = if network_type != "custom" {
        serde_json::Value::Null
    } else if resp["api_json"].is_object() {
        resp["api_json"].clone()
    } else {
        let mut api_map = serde_json::Map::new();
        api_map.insert("none".to_owned(), serde_json::json!("none"));
        let turn_host = resp["turn_host"].as_str().unwrap_or("");
        if !turn_host.is_empty() {
            let turn_protocol = resp["turn_protocol"].as_str().filter(|s| !s.is_empty()).unwrap_or("turn");
            api_map.insert("turnservers".to_owned(), serde_json::json!([{
                "protocol": turn_protocol,
                "host": turn_host,
                "port": resp["turn_port"].as_str().unwrap_or(""),
                "username": resp["turn_username"].as_str().unwrap_or(""),
                "password": resp["turn_password"].as_str().unwrap_or("")
            }]));
        }
        let rendezvous_host = resp["rendezvous_host"].as_str().unwrap_or("");
        if !rendezvous_host.is_empty() {
            api_map.insert("rendezvous".to_owned(), serde_json::json!({
                "host": rendezvous_host,
                "port": resp["rendezvous_port"].as_str().unwrap_or("")
            }));
        }
        let rendezvousssl_host = resp["rendezvousssl_host"].as_str().unwrap_or("");
        if !rendezvousssl_host.is_empty() {
            api_map.insert("rendezvousssl".to_owned(), serde_json::json!({
                "host": rendezvousssl_host,
                "port": resp["rendezvousssl_port"].as_str().unwrap_or("")
            }));
        }
        let signal_token = resp["signal_token"].as_str().unwrap_or("");
        if !signal_token.is_empty() {
            api_map.insert("signal_token".to_owned(), serde_json::json!(signal_token));
        }
        serde_json::Value::Object(api_map)
    };
    apply_network_settings(network_type, &api_json).await
}

const API_JSON_MANAGED_KEY: &str = "managed_by";
const API_JSON_MANAGED_VALUE: &str = "dashboard";
const SHARED_NETWORK_WATCH_SECS: f32 = 30.0;

lazy_static::lazy_static! {
    static ref LAST_SHARED_NETWORK: Mutex<Option<String>> = Mutex::new(None);
}

fn shared_api_json_path() -> std::path::PathBuf {
    Config::shared_path("api.json")
}

fn read_shared_network_store() -> Option<String> {
    std::fs::read_to_string(shared_api_json_path()).ok()
}

fn managed_api_json_content(api_json: &serde_json::Value) -> ResultType<String> {
    let mut managed = api_json.clone();
    if let Some(obj) = managed.as_object_mut() {
        obj.insert(
            API_JSON_MANAGED_KEY.to_owned(),
            serde_json::json!(API_JSON_MANAGED_VALUE),
        );
    }
    Ok(serde_json::to_string_pretty(&managed)?)
}

fn is_managed_api_json(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .map(|v| v[API_JSON_MANAGED_KEY] == API_JSON_MANAGED_VALUE)
        .unwrap_or(false)
}

fn remove_legacy_managed_api_json() {
    let legacy = Config::path("api.json");
    if legacy == shared_api_json_path() || !is_managed_api_json(&legacy) {
        return;
    }
    if let Err(e) = std::fs::remove_file(&legacy) {
        log::warn!("Failed to remove legacy network config {:?}: {}", legacy, e);
    }
}

fn write_file_creating_parent(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, content)
}

fn write_managed_api_json(api_json: &serde_json::Value) -> ResultType<bool> {
    let path = shared_api_json_path();
    let new_content = managed_api_json_content(api_json)?;
    if std::fs::read_to_string(&path).ok().as_deref() == Some(new_content.as_str()) {
        remove_legacy_managed_api_json();
        return Ok(false);
    }
    match write_file_creating_parent(&path, &new_content) {
        Ok(()) => {
            remove_legacy_managed_api_json();
            *LAST_SHARED_NETWORK.lock().unwrap() = Some(new_content);
            log::info!("Wrote custom network config to {:?}", path);
        }
        Err(e) => {
            let legacy = Config::path("api.json");
            if legacy == path {
                return Err(e.into());
            }
            log::warn!("Cannot write shared network config {:?} ({}), using {:?}", path, e, legacy);
            write_file_creating_parent(&legacy, &new_content)?;
        }
    }
    Ok(true)
}

fn remove_managed_api_json() -> bool {
    remove_legacy_managed_api_json();
    let path = shared_api_json_path();
    if !is_managed_api_json(&path) {
        return false;
    }
    if let Err(e) = std::fs::remove_file(&path) {
        log::warn!("Failed to remove network config {:?}: {}", path, e);
        return false;
    }
    *LAST_SHARED_NETWORK.lock().unwrap() = None;
    log::info!("Removed dashboard-managed network config {:?}", path);
    true
}

async fn reload_network_config() {
    Config::set_option("api-cache".to_owned(), String::new());
    hbb_common::api::reset_local_pin().await;
    hbb_common::api::erase_api().await;
    crate::rendezvous_mediator::RendezvousMediator::restart();
}

pub async fn apply_network_settings(
    network_type: &str,
    api_json: &serde_json::Value,
) -> ResultType<()> {
    let custom = network_type == "custom";
    if custom && !api_json.is_object() {
        log::warn!("Ignoring custom network settings without an api_json object");
        return Ok(());
    }

    let _guard = NETWORK_APPLY_LOCK.lock().await;
    let changed = if custom {
        write_managed_api_json(api_json)?
    } else {
        remove_managed_api_json()
    };
    if changed {
        reload_network_config().await;
    }
    Ok(())
}

pub fn seed_builtin_network(network: &serde_json::Value) {
    let api_json = &network["api_json"];
    if network["network_type"].as_str() != Some("custom") || !api_json.is_object() {
        return;
    }
    let Ok(content) = managed_api_json_content(api_json) else {
        return;
    };
    let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
    let marker = Config::shared_path("BuiltinNetwork.toml");
    let seeded = std::fs::read_to_string(&marker)
        .map(|s| s.trim() == digest)
        .unwrap_or(false);
    if seeded {
        return;
    }
    if let Err(e) = write_managed_api_json(api_json) {
        log::warn!("Failed to apply built-in network config: {}", e);
        return;
    }
    log::info!("Applied built-in network config");
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&marker, digest).ok();
}

async fn release_account_network() {
    let result = match crate::common::builtin_network() {
        Some(api_json) => apply_network_settings("custom", &api_json).await,
        None => apply_network_settings("hoptodesk", &serde_json::Value::Null).await,
    };
    if let Err(e) = result {
        log::warn!("Failed to release the account network: {}", e);
    }
}

async fn release_network_when_unlinked() {
    let other_process_linked =
        cfg!(not(any(target_os = "android", target_os = "ios"))) && has_enrollment_sentinel();
    if is_linked()
        || !get_invite_code().is_empty()
        || !get_prefilled_enrollment_token().is_empty()
        || other_process_linked
    {
        return;
    }
    release_account_network().await;
}

#[tokio::main(flavor = "current_thread")]
pub async fn release_network_if_unlinked() {
    release_network_when_unlinked().await;
}

fn start_shared_network_watch() {
    let current = read_shared_network_store();
    *LAST_SHARED_NETWORK.lock().unwrap() = current.clone();
    tokio::spawn(async move {
        if let Some(on_disk) = current.and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok()) {
            if hbb_common::api::call_api().await.ok().as_ref() != Some(&on_disk) {
                let _guard = NETWORK_APPLY_LOCK.lock().await;
                log::info!("Loaded network config differs from the shared store, reloading");
                reload_network_config().await;
            }
        }
        loop {
            hbb_common::sleep(SHARED_NETWORK_WATCH_SECS).await;
            let latest = read_shared_network_store();
            let changed = {
                let mut last = LAST_SHARED_NETWORK.lock().unwrap();
                if *last == latest {
                    false
                } else {
                    *last = latest;
                    true
                }
            };
            if changed {
                let _guard = NETWORK_APPLY_LOCK.lock().await;
                log::info!("Shared network config changed in another process, reloading");
                reload_network_config().await;
            }
        }
    });
}

fn network_settings_from_event(data: &serde_json::Value) -> Option<(String, serde_json::Value)> {
    let network_type = data["network_type"].as_str().unwrap_or("").to_string();
    if network_type.is_empty() {
        return None;
    }
    Some((network_type, data["api_json"].clone()))
}

pub fn get_prefilled_enrollment_token() -> String {
    std::fs::read_to_string(Config::shared_path("EnrollmentToken.toml"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
}

pub fn set_prefilled_enrollment_token(token: &str) {
    let token = token.trim();
    if token.is_empty() {
        return;
    }
    let p = Config::shared_path("EnrollmentToken.toml");
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&p, token);
}

pub async fn link_device() -> ResultType<()> {
    let invite_code = get_invite_code();
    let prefilled_token = get_prefilled_enrollment_token();

    if invite_code.is_empty() && prefilled_token.is_empty() {
        hbb_common::bail!("No invite code or enrollment token set");
    }

    let (enrollment_token, dashboard_user_id) = if !prefilled_token.is_empty() {
        log::info!("Using pre-supplied enrollment token");
        (prefilled_token, String::new())
    } else {
        log::info!("Linking device with invite code: {}...", &invite_code[..invite_code.len().min(8)]);
        let (t, u, _, name) = validate_invite(&invite_code).await?;
        if !name.is_empty() {
            Config::set_option("dashboard_account_name".to_owned(), name);
        }
        (t, u)
    };
    if !enrollment_token.is_empty() {
        log::info!("Got enrollment token for secure registration");
    }

    let mut device_id = Config::get_id();
    for _ in 0..15 {
        if !device_id.is_empty() {
            break;
        }
        hbb_common::sleep(1.0).await;
        device_id = Config::get_id();
    }
    if device_id.is_empty() {
        hbb_common::bail!("Device ID not available after waiting");
    }

    let device_name = crate::common::hostname();
    let os_name = std::env::consts::OS;
    let mac = get_mac_address();
    let resolved_user_id = register_device(&enrollment_token, &invite_code, &device_id, &device_name, os_name, &mac).await?;

    let final_user_id = if !resolved_user_id.is_empty() {
        resolved_user_id
    } else {
        dashboard_user_id
    };
    if !final_user_id.is_empty() {
        Config::set_option("dashboard_user_id".to_owned(), final_user_id.clone());
        Config::set_option("dashboard_device_id".to_owned(), device_id.clone());
        log::info!("Dashboard user ID stored: {}", final_user_id);
    }

    if !invite_code.is_empty() {
        if let Err(e) = get_network_settings(&invite_code).await {
            log::warn!("Failed to get network settings: {}", e);
        }
    }

    write_enrollment_sentinel("");
    Ok(())
}

fn enrollment_artifact_digest() -> String {
    let artifact = {
        let token = get_prefilled_enrollment_token();
        if token.is_empty() {
            get_invite_code()
        } else {
            token
        }
    };
    if artifact.is_empty() {
        return String::new();
    }
    let digest: [u8; 32] = Sha256::digest(artifact.as_bytes()).into();
    digest[..8].iter().map(|b| format!("{:02x}", b)).collect()
}

fn write_enrollment_sentinel(origin: &str) {
    let sentinel = Config::shared_path("Enrolled.toml");
    if let Some(parent) = sentinel.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut body = format!(
        "user_id={}\ndevice_id={}\n",
        get_dashboard_user_id(),
        Config::get_id()
    );
    let account_name = Config::get_option("dashboard_account_name");
    if !account_name.is_empty() {
        body.push_str(&format!("account_name={}\n", account_name));
    }
    if !origin.is_empty() {
        body.push_str(&format!("origin={}\n", origin));
    }
    std::fs::write(&sentinel, body).ok();
    let key = get_device_key();
    if !key.is_empty() {
        std::fs::write(sentinel_key_path(), key).ok();
    }
}

fn sentinel_key_path() -> std::path::PathBuf {
    Config::path("Enrolled.key")
}

fn refresh_sentinel_key() {
    let path = Config::shared_path("Enrolled.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let user_id = get_dashboard_user_id();
    let key = get_device_key();
    if key.is_empty()
        || !text.lines().any(|l| l.strip_prefix("user_id=").map(str::trim) == Some(user_id.as_str()))
        || std::fs::write(sentinel_key_path(), key).is_err()
    {
        return;
    }
    if text.lines().any(|l| l.starts_with("key=")) {
        let body: String = text
            .lines()
            .filter(|l| !l.starts_with("key="))
            .map(|l| format!("{}\n", l))
            .collect();
        std::fs::write(&path, body).ok();
    }
}

fn sentinel_origin() -> Option<String> {
    let text = std::fs::read_to_string(Config::shared_path("Enrolled.toml")).ok()?;
    let mut origin = String::new();
    let mut has_user_id = false;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("origin=") {
            origin = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("user_id=") {
            has_user_id = !v.trim().is_empty();
        }
    }
    if has_user_id {
        Some(origin)
    } else {
        None
    }
}

fn sentinel_supersedes_artifacts() -> bool {
    match sentinel_origin() {
        Some(origin) => origin.is_empty() || origin == enrollment_artifact_digest(),
        None => false,
    }
}

fn read_enrollment_sentinel() -> Option<(String, String, String, String)> {
    let text = std::fs::read_to_string(Config::shared_path("Enrolled.toml")).ok()?;
    let mut user_id = String::new();
    let mut device_id = String::new();
    let mut account_name = String::new();
    let mut key = String::new();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("user_id=") {
            user_id = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("device_id=") {
            device_id = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("account_name=") {
            account_name = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("key=") {
            key = v.trim().to_string();
        }
    }
    if let Ok(stored) = std::fs::read_to_string(sentinel_key_path()) {
        if !stored.trim().is_empty() {
            key = stored.trim().to_string();
        }
    }
    if user_id.is_empty() {
        None
    } else {
        Some((user_id, device_id, account_name, key))
    }
}

pub fn has_enrollment_sentinel() -> bool {
    read_enrollment_sentinel().is_some()
}

fn adopt_enrollment_sentinel() -> bool {
    match read_enrollment_sentinel() {
        Some((user_id, device_id, account_name, key)) => {
            log::info!(
                "Adopting dashboard link from enrollment sentinel (user_id={})",
                user_id
            );
            Config::set_option("dashboard_user_id".to_owned(), user_id);
            if !device_id.is_empty() {
                Config::set_option("dashboard_device_id".to_owned(), device_id);
            }
            if !account_name.is_empty() {
                Config::set_option("dashboard_account_name".to_owned(), account_name);
            }
            if !key.is_empty() {
                Config::set_option("dashboard-device-key".to_owned(), key);
            }
            true
        }
        None => false,
    }
}

pub fn remove_enrollment_artifacts() {
    std::fs::remove_file(Config::path("InviteCode.toml")).ok();
    std::fs::remove_file(Config::shared_path("InviteCode.toml")).ok();
    std::fs::remove_file(Config::shared_path("EnrollmentToken.toml")).ok();
    std::fs::remove_file(Config::shared_path("Enrolled.toml")).ok();
    std::fs::remove_file(sentinel_key_path()).ok();
}

pub fn percent_decode_path(s: &str) -> String {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                16,
            ) {
                result.push(byte);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).to_string()
}

const ATTACHMENT_MAGIC: &[u8; 4] = b"HTDE";

fn encrypt_attachment(data: &[u8], dashboard_user_id: &str) -> Vec<u8> {
    let key: [u8; 32] = Sha256::digest(dashboard_user_id.as_bytes()).into();
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(ATTACHMENT_MAGIC);
    for (i, &b) in data.iter().enumerate() {
        out.push(b ^ key[i % 32]);
    }
    out
}

pub fn decrypt_attachment(data: &[u8], dashboard_user_id: &str) -> Vec<u8> {
    if data.len() < 4 || &data[..4] != ATTACHMENT_MAGIC {
        return data.to_vec();
    }
    let key: [u8; 32] = Sha256::digest(dashboard_user_id.as_bytes()).into();
    let encoded = &data[4..];
    encoded.iter().enumerate().map(|(i, &b)| b ^ key[i % 32]).collect()
}

fn get_mac_address() -> String {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        mac_address::get_mac_address()
            .ok()
            .flatten()
            .map(|m| m.to_string())
            .unwrap_or_default()
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    "".to_string()
}

fn apply_pending_proxy() {
    let path = Config::shared_path("Proxy.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return,
    };
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
        let url = v["url"].as_str().unwrap_or("").trim().to_string();
        if !url.is_empty() {
            let username = v["username"].as_str().unwrap_or("").to_string();
            let password = v["password"].as_str().unwrap_or("").to_string();
            let (proxy_type, proxy) = if let Some(idx) = url.find("://") {
                let scheme = url[..idx].to_lowercase();
                let host = url[idx + 3..].to_string();
                let pt = match scheme.as_str() {
                    "http" | "https" => hbb_common::config::ProxyType::Http,
                    "socks5" | "socks" | "socks5h" => hbb_common::config::ProxyType::Socks5,
                    _ => hbb_common::config::ProxyType::Auto,
                };
                (pt, host)
            } else {
                (hbb_common::config::ProxyType::Auto, url.clone())
            };
            Config::set_socks(Some(hbb_common::config::Socks5Server {
                proxy,
                username,
                password,
                proxy_type,
            }));
            log::info!("Applied deployment proxy settings");
        }
    }
    let _ = std::fs::remove_file(&path);
}

#[tokio::main(flavor = "current_thread")]
pub async fn start() {
    if DASHBOARD_RUNNING.swap(true, Ordering::SeqCst) {
        log::info!("Dashboard connection already running");
        return;
    }

    if !is_linked() {
        release_device_policy();
        if !follows_service() {
            release_network_when_unlinked().await;
        }
    } else if crate::is_server() {
        refresh_sentinel_key();
    }

    apply_pending_proxy();

    refresh_dashboard_endpoints().await;

    let mut last_link_attempt: Option<tokio::time::Instant> = None;
    #[cfg(windows)]
    let mut last_wake: Option<tokio::time::Instant> = None;
    loop {
        #[cfg(windows)]
        if follows_service() {
            let local_invite = get_invite_code();
            mirror_service_link().await;
            if is_linked() {
                std::fs::remove_file(Config::path("InviteCode.toml")).ok();
                break;
            }
            let wake_due = last_wake.map_or(true, |t| t.elapsed().as_secs() >= LINK_RETRY_INTERVAL_SECS);
            if wake_due && (!local_invite.is_empty() || !get_prefilled_enrollment_token().is_empty()) {
                let woke = if !local_invite.is_empty() {
                    crate::ipc::notify_dashboard_relink_invite(&local_invite).await.is_ok()
                } else {
                    crate::ipc::notify_dashboard_relinked().await.is_ok()
                };
                if woke {
                    last_wake = Some(tokio::time::Instant::now());
                }
            }
            hbb_common::sleep(LINK_POLL_INTERVAL_SECS as _).await;
            continue;
        }
        let invite_code = get_invite_code();
        let has_artifacts =
            !invite_code.is_empty() || !get_prefilled_enrollment_token().is_empty();
        let artifacts_actionable = if is_linked() {
            !invite_code.is_empty()
                && invite_code != Config::get_option("last_enrolled_invite_code")
        } else {
            has_artifacts
        };
        if is_linked() && !artifacts_actionable {
            break;
        }
        let attempt_due = last_link_attempt
            .map(|t| t.elapsed().as_secs() >= LINK_RETRY_INTERVAL_SECS)
            .unwrap_or(true);
        if !is_linked() && sentinel_supersedes_artifacts() && adopt_enrollment_sentinel() {
            if !invite_code.is_empty() {
                Config::set_option("last_enrolled_invite_code".to_owned(), invite_code);
            }
            continue;
        }
        let artifact_digest = enrollment_artifact_digest();
        if artifacts_actionable && attempt_due {
            let Some(_link_lock) = acquire_link_lock() else {
                hbb_common::sleep(LINK_LOCK_WAIT_SECS).await;
                continue;
            };
            if last_link_attempt.is_some() {
                refresh_dashboard_endpoints().await;
            }
            last_link_attempt = Some(tokio::time::Instant::now());
            match link_device().await {
                Ok(()) => {
                    Config::set_option("dashboard_link_error".to_owned(), String::new());
                    Config::set_option("invite_code".to_owned(), String::new());
                    Config::set_option("last_enrolled_invite_code".to_owned(), invite_code);
                    std::fs::remove_file(Config::path("InviteCode.toml")).ok();
                    if crate::is_server() {
                        std::fs::remove_file(Config::shared_path("InviteCode.toml")).ok();
                        std::fs::remove_file(Config::shared_path("EnrollmentToken.toml")).ok();
                    }
                    write_enrollment_sentinel(&artifact_digest);
                    continue;
                }
                Err(e) => {
                    log::error!("Failed to link device: {}", e);
                    set_link_error(&e);
                    if is_device_limit_error(&e) || is_linked() {
                        break;
                    }
                }
            }
        } else if !is_linked() && adopt_enrollment_sentinel() {
            continue;
        }
        if is_linked() {
            break;
        }
        hbb_common::sleep(LINK_POLL_INTERVAL_SECS as _).await;
    }

    let dashboard_user_id = get_dashboard_user_id();
    if dashboard_user_id.is_empty() {
        log::info!("No dashboard_user_id, not starting WebSocket");
        DASHBOARD_RUNNING.store(false, Ordering::SeqCst);
        return;
    }

    // Restore registered device ID if config was reset
    let stored_device_id = Config::get_option("dashboard_device_id");
    if !stored_device_id.is_empty() && Config::get_id() != stored_device_id {
        log::warn!("Device ID changed from {} to {}, restoring registered ID", Config::get_id(), stored_device_id);
        Config::set_id(&stored_device_id);
        crate::rendezvous_mediator::RendezvousMediator::restart();
    }

    start_network_apply_worker();
    start_shared_network_watch();

    let enrolled_code = Config::get_option("last_enrolled_invite_code");
    if !enrolled_code.is_empty() {
        match tokio::time::timeout(
            Duration::from_secs(20),
            get_network_settings(&enrolled_code),
        )
        .await
        {
            Ok(Err(e)) => log::warn!("Startup network settings refresh failed: {}", e),
            Err(_) => log::warn!("Startup network settings refresh timed out"),
            Ok(Ok(())) => {}
        }
    }

    log::info!("Starting dashboard WebSocket connection");
    let mut reconnect_delay = RECONNECT_DELAY_BASE_SECS;

    loop {
        let connected_at = tokio::time::Instant::now();
        match dashboard_ws_loop(&get_dashboard_user_id()).await {
            Ok(()) => {
                log::info!("Dashboard WebSocket loop ended normally");
                reconnect_delay = RECONNECT_DELAY_BASE_SECS;
            }
            Err(e) => {
                log::error!("Dashboard WebSocket error: {}", e);
            }
        }
        if connected_at.elapsed() > Duration::from_secs(RECONNECT_HEALTHY_RESET_SECS) {
            reconnect_delay = RECONNECT_DELAY_BASE_SECS;
        }

        if get_dashboard_user_id().is_empty() {
            log::info!("Device unlinked from dashboard, stopping reconnection");
            release_device_policy();
            if !follows_service() {
                release_account_network().await;
            }
            DASHBOARD_RUNNING.store(false, Ordering::SeqCst);
            break;
        }

        let jittered_delay = hbb_common::rand::Rng::gen_range(
            &mut hbb_common::rand::thread_rng(),
            0.5f32..=reconnect_delay as f32,
        );
        log::info!("Reconnecting dashboard WebSocket in {:.1}s...", jittered_delay);
        hbb_common::sleep(jittered_delay).await;
        reconnect_delay = (reconnect_delay * 2).min(RECONNECT_DELAY_MAX_SECS);
    }
}

async fn dashboard_ws_loop(dashboard_user_id: &str) -> ResultType<()> {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let url = format!(
        "{}?dashboard_user_id={}&EIO=4&transport=websocket",
        dashboard_ws_url(), dashboard_user_id
    );
    log::info!("Connecting to dashboard WebSocket");

    let tls_opts = Some(tokio_tungstenite::Connector::NativeTls(
        native_tls::TlsConnector::builder()
            .request_alpns(&["http/1.1"])
            .build()?,
    ));

    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::header::{HeaderValue, ACCEPT_LANGUAGE, USER_AGENT};
    let mut request = url.as_str().into_client_request()?;
    {
        let headers = request.headers_mut();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
            ),
        );
        headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
    }

    let (ws_stream, _) =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, tls_opts)
            .await?;

    log::info!("Dashboard WebSocket connected");
    let (mut sender, mut receiver) = ws_stream.split();

    let open_msg = receiver
        .next()
        .await
        .ok_or_else(|| hbb_common::anyhow::anyhow!("WS closed before open packet"))??;
    let open_text = open_msg.to_text()?;
    if !open_text.starts_with('0') {
        hbb_common::bail!("Expected Engine.IO open packet, got: {}", open_text);
    }

    send_ws_message(&mut sender, WsMessage::Text("40/device,".to_string())).await?;

    let ack_msg = receiver
        .next()
        .await
        .ok_or_else(|| hbb_common::anyhow::anyhow!("WS closed before namespace ACK"))??;
    let ack_text = ack_msg.to_text()?;
    if !ack_text.starts_with("40/device") {
        hbb_common::bail!("Expected namespace ACK, got: {}", ack_text);
    }

    let device_id = Config::get_id();
    let computer_name = crate::common::hostname();
    let os_name = std::env::consts::OS;
    let timezone = get_timezone();
    let mac = get_mac_address();
    let local_ip = hbb_common::socket_client::get_lan_ipv4()
        .map(|ip| ip.to_string())
        .unwrap_or_default();
    let wol_enabled = !crate::ui_interface::get_option("wol-enabled").is_empty();

    let follower = follows_service();
    let device_key = device_key(follower).await;
    let register_data = serde_json::json!({
        "device_id": device_id,
        "dashboard_user_id": dashboard_user_id,
        "key_capable": !follower,
        "key_scheme": KEY_SCHEME,
        "device_key": device_key,
        "timezone": timezone,
        "requested_name": Config::get_option("pending-device-name"),
        "computer_name": computer_name,
        "os": os_name,
        "mac_address": mac,
        "local_ip": local_ip,
        "wol_enabled": wol_enabled,
        "two_fa": !Config::get_option("2fa").is_empty(),
        "desk_capable": cfg!(not(any(target_os = "android", target_os = "ios")))
    });

    hbb_common::sleep(0.5).await;
    send_socketio_event(&mut sender, "register", &register_data).await?;

    let mut heartbeat_timer =
        crate::rustdesk_interval(tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS)));
    let mut outbox_timer =
        crate::rustdesk_interval(tokio::time::interval(Duration::from_secs(OUTBOX_DRAIN_INTERVAL_SECS)));
    let mut liveness_timer =
        crate::rustdesk_interval(tokio::time::interval(Duration::from_secs(WS_LIVENESS_CHECK_SECS)));
    let mut was_in_session = false;
    let mut last_ws_data = tokio::time::Instant::now();
    const WS_READ_TIMEOUT_SECS: u64 = 240;

    let mut metrics_sys = hbb_common::sysinfo::System::new();
    #[cfg(not(windows))]
    metrics_sys.refresh_cpu();
    #[cfg(windows)]
    windows_cpu_pct();

    let (mcp_tx, mut mcp_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, serde_json::Value)>();

    loop {
        select! {
            msg = receiver.next() => {
                last_ws_data = tokio::time::Instant::now();
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        if text == "2" {
                            send_ws_message(&mut sender, WsMessage::Text("3".to_string())).await?;
                            continue;
                        }
                        if text == "3" {
                            continue;
                        }
                        if let Some((resp_event, resp_data)) = handle_incoming_message(&text, &mcp_tx)? {
                            send_socketio_event(&mut sender, &resp_event, &resp_data).await?;
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        log::info!("Dashboard WebSocket closed by server");
                        break;
                    }
                    Some(Err(e)) => {
                        hbb_common::bail!("Dashboard WebSocket error: {}", e);
                    }
                    None => {
                        log::info!("Dashboard WebSocket stream ended");
                        break;
                    }
                    _ => {}
                }
            }
            _ = liveness_timer.tick() => {
                if last_ws_data.elapsed() > Duration::from_secs(WS_READ_TIMEOUT_SECS) {
                    log::warn!("Dashboard WebSocket: no data received for {}s, reconnecting", WS_READ_TIMEOUT_SECS);
                    break;
                }
                if get_dashboard_user_id() != dashboard_user_id {
                    log::info!("Dashboard link changed, closing WebSocket");
                    break;
                }
            }
            _ = heartbeat_timer.tick() => {
                report_session_transition(&mut sender, &device_id, &mut was_in_session).await?;
                let current_in_session = IN_SESSION.load(Ordering::Relaxed);

                let wol_now = !crate::ui_interface::get_option("wol-enabled").is_empty();
                let (cpu_pct, mem_pct) = collect_device_metrics(&mut metrics_sys);
                let (disk_pct, disk_free_mb, disks) = collect_disk_usage();
                let (battery_pct, on_ac) = collect_power_state();
                let (uptime_secs, boot_time) = collect_uptime_boot(&metrics_sys);
                let temps = collect_temperatures();
                let custom_build_id = hbb_common::config::custom_build_id().parse::<u64>().ok();
                let heartbeat = serde_json::json!({
                    "device_id": device_id,
                    "timezone": timezone,
                    "in_session": current_in_session,
                    "wol_enabled": wol_now,
                    "cpu_pct": cpu_pct,
                    "mem_pct": mem_pct,
                    "disk_pct": disk_pct,
                    "disk_free_mb": disk_free_mb,
                    "disks": disks,
                    "battery_pct": battery_pct,
                    "on_ac": on_ac,
                    "uptime_secs": uptime_secs,
                    "boot_time": boot_time,
                    "temps": temps,
                    "version": crate::VERSION,
                    "build_id": custom_build_id,
                    "app_name": if custom_build_id.is_some() { crate::get_app_name() } else { String::new() }
                });
                send_socketio_event(&mut sender, "heartbeat", &heartbeat).await?;

                let pending_name = Config::get_option("pending-device-name");
                if !pending_name.is_empty() {
                    send_socketio_event(&mut sender, "device:set_name", &serde_json::json!({
                        "device_id": device_id,
                        "name": pending_name
                    })).await?;
                }

                drain_note_and_recording_outboxes(&mut sender, &device_id).await?;
            }
            _ = outbox_timer.tick() => {
                drain_note_and_recording_outboxes(&mut sender, &device_id).await?;
            }
            Some((mcp_event, mcp_data)) = mcp_rx.recv() => {
                send_socketio_event(&mut sender, &mcp_event, &mcp_data).await?;
            }
            _ = SESSION_NOTIFY.notified() => {
                report_session_transition(&mut sender, &device_id, &mut was_in_session).await?;
            }
        }
    }

    Ok(())
}

#[cfg(windows)]
fn windows_cpu_pct() -> i64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetSystemTimes;
    static PREV: Mutex<Option<(u64, u64)>> = Mutex::new(None);
    fn to_u64(t: FILETIME) -> u64 {
        ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64
    }
    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.is_err() {
        return 0;
    }
    let idle = to_u64(idle);
    let total = to_u64(kernel) + to_u64(user);
    let mut prev = PREV.lock().unwrap();
    let pct = match *prev {
        Some((prev_idle, prev_total)) if total > prev_total => {
            let total_delta = total - prev_total;
            let busy_delta = total_delta.saturating_sub(idle.saturating_sub(prev_idle));
            ((busy_delta as f64 / total_delta as f64) * 100.0)
                .round()
                .clamp(0.0, 100.0) as i64
        }
        _ => 0,
    };
    *prev = Some((idle, total));
    pct
}

fn collect_device_metrics(sys: &mut hbb_common::sysinfo::System) -> (i64, i64) {
    #[cfg(not(windows))]
    sys.refresh_cpu();
    sys.refresh_memory();
    #[cfg(windows)]
    let cpu_pct = windows_cpu_pct();
    #[cfg(not(windows))]
    let cpu_pct = sys.global_cpu_info().cpu_usage().round().clamp(0.0, 100.0) as i64;
    let total_mem = sys.total_memory();
    let mem_pct = if total_mem > 0 {
        ((sys.used_memory() as f64 / total_mem as f64) * 100.0).round().clamp(0.0, 100.0) as i64
    } else {
        0
    };
    (cpu_pct, mem_pct)
}

fn collect_uptime_boot(sys: &hbb_common::sysinfo::System) -> (i64, i64) {
    (sys.uptime() as i64, sys.boot_time() as i64)
}

fn hotter(hottest: Option<f32>, celsius: f32) -> Option<f32> {
    if !celsius.is_finite() || celsius <= 0.0 || celsius >= 150.0 {
        return hottest;
    }
    Some(hottest.map_or(celsius, |h| h.max(celsius)))
}

fn temperatures_json(cpu: Option<f32>, gpu: Option<f32>, disk: Option<f32>) -> serde_json::Value {
    let round = |t: Option<f32>| t.map(|t| t.round() as i64);
    serde_json::json!({ "cpu": round(cpu), "gpu": round(gpu), "disk": round(disk) })
}

#[cfg(not(windows))]
fn sensor_kind(label: &str) -> Option<usize> {
    let label = label.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| label.contains(w));
    if has(&["nvme", "drivetemp", "nand", "ssd"]) {
        Some(2)
    } else if has(&["gpu", "nouveau", "radeon"]) {
        Some(1)
    } else if has(&["cpu", "coretemp", "k10temp", "zenpower", "tdie", "peci"]) {
        Some(0)
    } else {
        None
    }
}

#[cfg(not(windows))]
fn collect_temperatures() -> serde_json::Value {
    let mut hottest = [None; 3];
    for component in hbb_common::sysinfo::Components::new_with_refreshed_list().list() {
        if let Some(kind) = sensor_kind(component.label()) {
            hottest[kind] = hotter(hottest[kind], component.temperature());
        }
    }
    temperatures_json(hottest[0], hottest[1], hottest[2])
}

#[cfg(windows)]
fn collect_temperatures() -> serde_json::Value {
    temperatures_json(windows_thermal_zone_temp(), windows_gpu_temp(), windows_drive_temp())
}

#[cfg(windows)]
fn windows_thermal_zone_temp() -> Option<f32> {
    use windows::core::{w, PCWSTR};
    use windows::Win32::System::Performance::*;
    const VALID_DATA: [u32; 2] = [0, 1];
    let mut hottest = None;
    unsafe {
        let mut query = PDH_HQUERY::default();
        if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
            return None;
        }
        let mut counter = PDH_HCOUNTER::default();
        if PdhAddEnglishCounterW(query, w!("\\Thermal Zone Information(*)\\Temperature"), 0, &mut counter) == 0
            && PdhCollectQueryData(query) == 0
        {
            let (mut size, mut count) = (0u32, 0u32);
            PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, None);
            let mut buf = vec![0u64; (size as usize + 7) / 8];
            if size > 0
                && PdhGetFormattedCounterArrayW(
                    counter,
                    PDH_FMT_DOUBLE,
                    &mut size,
                    &mut count,
                    Some(buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W),
                ) == 0
            {
                let items = std::slice::from_raw_parts(
                    buf.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W,
                    count as usize,
                );
                for item in items {
                    if VALID_DATA.contains(&item.FmtValue.CStatus) {
                        hottest = hotter(hottest, (item.FmtValue.Anonymous.doubleValue - 273.15) as f32);
                    }
                }
            }
        }
        PdhCloseQuery(query);
    }
    hottest
}

#[cfg(windows)]
fn windows_gpu_temp() -> Option<f32> {
    use windows::Wdk::Graphics::Direct3D::*;
    use windows::Win32::Foundation::NTSTATUS;
    type EnumAdapters = unsafe extern "system" fn(*mut D3DKMT_ENUMADAPTERS2) -> NTSTATUS;
    type QueryAdapterInfo = unsafe extern "system" fn(*mut D3DKMT_QUERYADAPTERINFO) -> NTSTATUS;
    type CloseAdapter = unsafe extern "system" fn(*const D3DKMT_CLOSEADAPTER) -> NTSTATUS;
    let mut hottest = None;
    unsafe {
        let gdi = hbb_common::libloading::Library::new("gdi32.dll").ok()?;
        let enum_adapters = gdi.get::<EnumAdapters>(b"D3DKMTEnumAdapters2\0").ok()?;
        let query_info = gdi.get::<QueryAdapterInfo>(b"D3DKMTQueryAdapterInfo\0").ok()?;
        let close_adapter = gdi.get::<CloseAdapter>(b"D3DKMTCloseAdapter\0").ok()?;
        let mut list = D3DKMT_ENUMADAPTERS2::default();
        if enum_adapters(&mut list).is_err() || list.NumAdapters == 0 {
            return None;
        }
        let mut adapters = vec![D3DKMT_ADAPTERINFO::default(); list.NumAdapters as usize];
        list.pAdapters = adapters.as_mut_ptr();
        if enum_adapters(&mut list).is_err() {
            return None;
        }
        adapters.truncate(list.NumAdapters as usize);
        for adapter in &adapters {
            let mut perf = D3DKMT_ADAPTER_PERFDATA::default();
            let mut info = D3DKMT_QUERYADAPTERINFO {
                hAdapter: adapter.hAdapter,
                Type: KMTQAITYPE_ADAPTERPERFDATA,
                pPrivateDriverData: &mut perf as *mut D3DKMT_ADAPTER_PERFDATA as *mut core::ffi::c_void,
                PrivateDriverDataSize: std::mem::size_of::<D3DKMT_ADAPTER_PERFDATA>() as u32,
            };
            if query_info(&mut info).is_ok() {
                hottest = hotter(hottest, perf.Temperature as f32 / 10.0);
            }
            let _ = close_adapter(&D3DKMT_CLOSEADAPTER { hAdapter: adapter.hAdapter });
        }
    }
    hottest
}

#[cfg(windows)]
fn windows_drive_temp() -> Option<f32> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    const MAX_DRIVES: u32 = 16;
    let mut hottest = None;
    for n in 0..MAX_DRIVES {
        let path: Vec<u16> = format!("\\\\.\\PhysicalDrive{n}").encode_utf16().chain(Some(0)).collect();
        unsafe {
            let Ok(handle) = CreateFileW(
                PCWSTR(path.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            ) else {
                continue;
            };
            if let Some(celsius) = drive_sensor_temp(handle).or_else(|| nvme_health_temp(handle)) {
                hottest = hotter(hottest, celsius);
            }
            let _ = CloseHandle(handle);
        }
    }
    hottest
}

#[cfg(windows)]
unsafe fn storage_query<I, O>(handle: windows::Win32::Foundation::HANDLE, input: &I, output: &mut O) -> Option<usize> {
    use windows::Win32::System::Ioctl::IOCTL_STORAGE_QUERY_PROPERTY;
    use windows::Win32::System::IO::DeviceIoControl;
    let mut returned = 0u32;
    DeviceIoControl(
        handle,
        IOCTL_STORAGE_QUERY_PROPERTY,
        Some(input as *const I as *const core::ffi::c_void),
        std::mem::size_of::<I>() as u32,
        Some(output as *mut O as *mut core::ffi::c_void),
        std::mem::size_of::<O>() as u32,
        Some(&mut returned),
        None,
    )
    .ok()?;
    Some(returned as usize)
}

#[cfg(windows)]
unsafe fn drive_sensor_temp(handle: windows::Win32::Foundation::HANDLE) -> Option<f32> {
    use windows::Win32::System::Ioctl::*;
    const MAX_SENSORS: usize = 16;
    #[repr(C)]
    struct Reply {
        descriptor: STORAGE_TEMPERATURE_DATA_DESCRIPTOR,
        more: [STORAGE_TEMPERATURE_INFO; MAX_SENSORS - 1],
    }
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceTemperatureProperty,
        QueryType: PropertyStandardQuery,
        ..Default::default()
    };
    let mut reply: Reply = std::mem::zeroed();
    let returned = storage_query(handle, &query, &mut reply)?;
    let info_size = std::mem::size_of::<STORAGE_TEMPERATURE_INFO>();
    let header = std::mem::size_of::<STORAGE_TEMPERATURE_DATA_DESCRIPTOR>() - info_size;
    let count = (reply.descriptor.InfoCount as usize)
        .min(returned.saturating_sub(header) / info_size)
        .min(MAX_SENSORS);
    std::slice::from_raw_parts(reply.descriptor.TemperatureInfo.as_ptr(), count)
        .iter()
        .fold(None, |hottest, info| hotter(hottest, info.Temperature as f32))
}

#[cfg(windows)]
unsafe fn nvme_health_temp(handle: windows::Win32::Foundation::HANDLE) -> Option<f32> {
    use windows::Win32::System::Ioctl::*;
    const HEALTH_INFO_LOG: u32 = 2;
    #[repr(C)]
    struct HealthLog {
        property: STORAGE_PROPERTY_ID,
        query_type: STORAGE_QUERY_TYPE,
        protocol: STORAGE_PROTOCOL_SPECIFIC_DATA,
        log: [u8; 512],
    }
    let mut query: HealthLog = std::mem::zeroed();
    query.property = StorageDeviceProtocolSpecificProperty;
    query.query_type = PropertyStandardQuery;
    query.protocol.ProtocolType = ProtocolTypeNvme;
    query.protocol.DataType = NVMeDataTypeLogPage.0 as u32;
    query.protocol.ProtocolDataRequestValue = HEALTH_INFO_LOG;
    query.protocol.ProtocolDataOffset = std::mem::size_of::<STORAGE_PROTOCOL_SPECIFIC_DATA>() as u32;
    query.protocol.ProtocolDataLength = query.log.len() as u32;
    let mut reply: HealthLog = std::mem::zeroed();
    storage_query(handle, &query, &mut reply)?;
    if reply.protocol.ProtocolDataOffset != query.protocol.ProtocolDataOffset
        || reply.protocol.ProtocolDataLength < 3
    {
        return None;
    }
    let kelvin = u16::from_le_bytes([reply.log[1], reply.log[2]]);
    hotter(None, kelvin as f32 - 273.15)
}

#[cfg(target_os = "android")]
fn collect_power_state() -> (i64, i64) {
    (scrap::android::get_device_battery() as i64, -1)
}

#[cfg(windows)]
fn collect_power_state() -> (i64, i64) {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    const NO_SYSTEM_BATTERY: u8 = 128;
    let mut status = SYSTEM_POWER_STATUS::default();
    if unsafe { GetSystemPowerStatus(&mut status) }.is_err()
        || status.BatteryFlag & NO_SYSTEM_BATTERY != 0
    {
        return (-1, -1);
    }
    let battery_pct = if status.BatteryLifePercent <= 100 {
        status.BatteryLifePercent as i64
    } else {
        -1
    };
    let on_ac = match status.ACLineStatus {
        0 => 0,
        1 => 1,
        _ => -1,
    };
    (battery_pct, on_ac)
}

#[cfg(target_os = "macos")]
fn collect_power_state() -> (i64, i64) {
    use core_foundation::{
        array::{CFArray, CFArrayRef},
        base::{CFType, CFTypeRef, TCFType},
        dictionary::{CFDictionary, CFDictionaryRef},
        number::CFNumber,
        string::CFString,
    };
    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
        fn IOPSCopyPowerSourcesList(blob: CFTypeRef) -> CFArrayRef;
        fn IOPSGetPowerSourceDescription(blob: CFTypeRef, source: CFTypeRef) -> CFDictionaryRef;
    }
    unsafe {
        let blob = IOPSCopyPowerSourcesInfo();
        if blob.is_null() {
            return (-1, -1);
        }
        let blob = CFType::wrap_under_create_rule(blob);
        let sources = IOPSCopyPowerSourcesList(blob.as_CFTypeRef());
        if sources.is_null() {
            return (-1, -1);
        }
        let sources = CFArray::<CFType>::wrap_under_create_rule(sources);
        for source in sources.iter() {
            let desc = IOPSGetPowerSourceDescription(blob.as_CFTypeRef(), source.as_CFTypeRef());
            if desc.is_null() {
                continue;
            }
            let desc = CFDictionary::<CFString, CFType>::wrap_under_get_rule(desc);
            let text = |key: &'static str| {
                desc.find(CFString::from_static_string(key))
                    .and_then(|v| v.downcast::<CFString>())
                    .map(|s| s.to_string())
            };
            let number = |key: &'static str| {
                desc.find(CFString::from_static_string(key))
                    .and_then(|v| v.downcast::<CFNumber>())
                    .and_then(|n| n.to_i64())
            };
            if text("Type").as_deref() != Some("InternalBattery") {
                continue;
            }
            let battery_pct = match (number("Current Capacity"), number("Max Capacity")) {
                (Some(current), Some(max)) if max > 0 => {
                    ((current as f64 / max as f64) * 100.0).round().clamp(0.0, 100.0) as i64
                }
                _ => -1,
            };
            let on_ac = match text("Power Source State").as_deref() {
                Some("AC Power") => 1,
                Some("Battery Power") => 0,
                _ => -1,
            };
            return (battery_pct, on_ac);
        }
    }
    (-1, -1)
}

#[cfg(target_os = "linux")]
fn collect_power_state() -> (i64, i64) {
    let Ok(entries) = std::fs::read_dir("/sys/class/power_supply") else {
        return (-1, -1);
    };
    let mut battery_found = false;
    let mut capacities = Vec::new();
    let mut battery_status_on_ac = -1;
    let mut supply_found = false;
    let mut supply_online = false;
    for entry in entries.flatten() {
        let path = entry.path();
        let read = |name: &str| {
            std::fs::read_to_string(path.join(name))
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };
        if read("scope") == "Device" {
            continue;
        }
        match read("type").as_str() {
            "Battery" => {
                battery_found = true;
                if let Ok(pct) = read("capacity").parse::<i64>() {
                    capacities.push(pct.clamp(0, 100));
                }
                match read("status").as_str() {
                    "Discharging" => battery_status_on_ac = 0,
                    "Charging" | "Full" | "Not charging" if battery_status_on_ac != 0 => {
                        battery_status_on_ac = 1
                    }
                    _ => {}
                }
            }
            "Mains" | "USB" => {
                supply_found = true;
                supply_online |= read("online") == "1";
            }
            _ => {}
        }
    }
    if !battery_found {
        return (-1, -1);
    }
    let battery_pct = if capacities.is_empty() {
        -1
    } else {
        capacities.iter().sum::<i64>() / capacities.len() as i64
    };
    let on_ac = if supply_online {
        1
    } else if battery_status_on_ac >= 0 {
        battery_status_on_ac
    } else if supply_found {
        0
    } else {
        -1
    };
    (battery_pct, on_ac)
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux", target_os = "android")))]
fn collect_power_state() -> (i64, i64) {
    (-1, -1)
}

#[cfg(target_os = "android")]
fn collect_disk_usage() -> (i64, i64, Vec<serde_json::Value>) {
    let dir = hbb_common::config::APP_DIR.read().unwrap().clone();
    let dir = if dir.is_empty() { "/data".to_string() } else { dir };
    let cpath = match std::ffi::CString::new(dir) {
        Ok(c) => c,
        Err(_) => return (0, -1, Vec::new()),
    };
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(cpath.as_ptr(), &mut stat) == 0 {
            let frsize = stat.f_frsize as u64;
            let total = stat.f_blocks as u64 * frsize;
            let avail = stat.f_bavail as u64 * frsize;
            if total > 0 {
                let used = total.saturating_sub(avail);
                let pct = ((used as f64 / total as f64) * 100.0).round() as i64;
                return (pct, (avail / 1_048_576) as i64, Vec::new());
            }
        }
    }
    (0, -1, Vec::new())
}

#[cfg(not(target_os = "android"))]
fn collect_disk_usage() -> (i64, i64, Vec<serde_json::Value>) {
    use hbb_common::sysinfo::Disks;
    const SKIPPED_FILE_SYSTEMS: [&str; 6] = ["cdfs", "udf", "iso9660", "cd9660", "squashfs", "overlay"];
    const SYSTEM_PARTITIONS: [&str; 3] = ["/recovery", "/boot/efi", "/efi"];
    const MAX_DISKS: usize = 16;
    let mut fullest = (0i64, -1i64);
    let mut disks = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for d in Disks::new_with_refreshed_list().list() {
        let total = d.total_space();
        let mount = d.mount_point().to_string_lossy().trim_end_matches('\\').to_string();
        if total < 1_000_000_000
            || d.is_removable()
            || SKIPPED_FILE_SYSTEMS.iter().any(|fs| d.file_system().eq_ignore_ascii_case(fs))
            || SYSTEM_PARTITIONS.contains(&mount.as_str())
            || mount.contains("/.mount_")
            || (cfg!(target_os = "linux") && !seen.insert(d.name().to_os_string()))
        {
            continue;
        }
        let available = d.available_space();
        let used = total.saturating_sub(available);
        let pct = ((used as f64 / total as f64) * 100.0).round() as i64;
        if fullest.1 < 0 || pct > fullest.0 {
            fullest = (pct, (available / 1_048_576) as i64);
        }
        if disks.len() < MAX_DISKS {
            disks.push(serde_json::json!({
                "mount": mount,
                "total_mb": total / 1_048_576,
                "free_mb": available / 1_048_576,
                "system": is_system_mount(&mount),
            }));
        }
    }
    (fullest.0, fullest.1, disks)
}

#[cfg(windows)]
fn is_system_mount(mount: &str) -> bool {
    std::env::var("SystemDrive").map_or(false, |drive| !drive.is_empty() && mount.eq_ignore_ascii_case(&drive))
}

#[cfg(not(any(windows, target_os = "android")))]
fn is_system_mount(mount: &str) -> bool {
    mount == "/"
}

async fn send_ws_message<S>(
    sender: &mut S,
    msg: tokio_tungstenite::tungstenite::Message,
) -> ResultType<()>
where
    S: futures::Sink<tokio_tungstenite::tungstenite::Message, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    use futures::SinkExt;
    match tokio::time::timeout(Duration::from_secs(WS_SEND_TIMEOUT_SECS), sender.send(msg)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => hbb_common::bail!("WS send timed out after {}s", WS_SEND_TIMEOUT_SECS),
    }
}

async fn drain_note_and_recording_outboxes<S>(sender: &mut S, device_id: &str) -> ResultType<()>
where
    S: futures::Sink<tokio_tungstenite::tungstenite::Message, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    let (note_content, note_entries) = take_session_notes();
    if !note_content.is_empty() {
        for (note, billable, noted_at, peer_id) in note_entries {
            send_socketio_event(sender, "session:note", &serde_json::json!({
                "device_id": device_id,
                "peer_id": peer_id,
                "note": note,
                "billable": billable,
                "noted_at": noted_at
            })).await?;
        }
        clear_session_notes(&note_content);
    }

    let (outbox_content, outbox_entries) = take_recording_outbox();
    if !outbox_content.is_empty() {
        for entry in outbox_entries {
            let mut payload = entry;
            payload["device_id"] = serde_json::json!(device_id);
            send_socketio_event(sender, "recording", &payload).await?;
        }
        clear_recording_outbox(&outbox_content);
    }

    if follows_service() {
        return Ok(());
    }
    let (tamper_content, tamper_reasons) = take_tamper_attempts();
    if !tamper_content.is_empty() {
        for reason in tamper_reasons {
            send_socketio_event(sender, "tamper_attempt", &serde_json::json!({
                "device_id": device_id,
                "reason": reason
            })).await?;
        }
        clear_tamper_attempts(&tamper_content);
    }
    Ok(())
}

async fn report_session_transition<S>(
    sender: &mut S,
    device_id: &str,
    was_in_session: &mut bool,
) -> ResultType<()>
where
    S: futures::Sink<tokio_tungstenite::tungstenite::Message, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    let current_in_session = IN_SESSION.load(Ordering::Relaxed);
    if current_in_session == *was_in_session {
        return Ok(());
    }
    let (stype, rip, issue, pname) = SESSION_META.lock().unwrap().clone();
    let mut payload = serde_json::json!({
        "device_id": device_id,
        "session_type": if stype.is_empty() { "screen".to_string() } else { stype },
        "remote_ip": rip,
        "timestamp": chrono::Utc::now().timestamp() as u64
    });
    if current_in_session && !issue.is_empty() {
        payload["issue"] = serde_json::json!(issue);
    }
    if current_in_session && !pname.is_empty() {
        payload["peer_name"] = serde_json::json!(pname);
    }
    let event = if current_in_session {
        "remote_session_start"
    } else {
        "remote_session_end"
    };
    send_socketio_event(sender, event, &payload).await?;
    *was_in_session = current_in_session;
    Ok(())
}

async fn send_socketio_event<S>(
    sender: &mut S,
    event: &str,
    data: &serde_json::Value,
) -> ResultType<()>
where
    S: futures::Sink<tokio_tungstenite::tungstenite::Message, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    let payload = format!("42/device,[{},{}]", serde_json::json!(event), data);
    send_ws_message(sender, tokio_tungstenite::tungstenite::Message::Text(payload)).await
}

fn handle_incoming_message(
    text: &str,
    _mcp_tx: &tokio::sync::mpsc::UnboundedSender<(String, serde_json::Value)>,
) -> ResultType<Option<(String, serde_json::Value)>> {
    if let Some(json_str) = text.strip_prefix("42/device,") {
        if let Ok(arr) = serde_json::from_str::<serde_json::Value>(json_str) {
            if let Some(event) = arr.get(0).and_then(|v| v.as_str()) {
                match event {
                    "registered" => {
                        if let Some(data) = arr.get(1) {
                            if let Some(uid) = data["dashboard_user_id"].as_str() {
                                if !uid.is_empty() && get_dashboard_user_id().is_empty() {
                                    Config::set_option("dashboard_user_id".to_owned(), uid.to_string());
                                    log::info!("Dashboard: stored dashboard_user_id from WS register ACK");
                                }
                            }
                            if store_device_key(&data["device_key"]) {
                                refresh_sentinel_key();
                            }
                            if !data["account_access_only"].is_null() {
                                let locked = data["account_access_only"].as_i64() == Some(1)
                                    || data["account_access_only"].as_bool() == Some(true);
                                apply_access_lock(locked);
                            }
                            if !data["easy_access"].is_null() {
                                apply_easy_access(
                                    data["easy_access"].as_i64() == Some(1)
                                        || data["easy_access"].as_bool() == Some(true),
                                );
                            }
                            if !data["device_policy"].is_null() {
                                apply_device_policy(&data["device_policy"]);
                            }
                            if let Some(desk) = data.get("desk") {
                                crate::desk::store(desk);
                            }
                            if let Some(name) = data["device_name"].as_str() {
                                if !name.is_empty() {
                                    Config::set_option("dashboard_device_name".to_owned(), name.to_string());
                                    if Config::get_option("pending-device-name") == name {
                                        Config::set_option("pending-device-name".to_owned(), String::new());
                                    }
                                }
                            }
                            if !data["session_notes"].is_null() {
                                let enabled = data["session_notes"].as_i64() == Some(1)
                                    || data["session_notes"].as_bool() == Some(true);
                                apply_session_notes(enabled);
                            }
                            if Config::get_option("dashboard-log-token").is_empty() {
                                return Ok(Some((
                                    "log_token:request".to_string(),
                                    serde_json::json!({}),
                                )));
                            }
                        }
                    }
                    "log_token" => {
                        if let Some(data) = arr.get(1) {
                            if let Some(token) = data["token"].as_str() {
                                if !token.is_empty() {
                                    Config::set_option("dashboard-log-token".to_owned(), token.to_string());
                                }
                            }
                        }
                    }
                    "heartbeat_ack" | "recording:ack" => {}
                    "error" => {
                        if let Some(data) = arr.get(1) {
                            if data["code"].as_str() == Some("unknown_account")
                                && data["dashboard_region"].is_object()
                            {
                                apply_region_from_response(data);
                                log::warn!("Dashboard: account lives in another region, reconnecting there");
                                hbb_common::bail!("Dashboard region changed");
                            }
                            if let Some(code) = data["code"].as_str() {
                                if DEVICE_KEY_ERROR_CODES.contains(&code) {
                                    if !follows_service() {
                                        forget_dashboard_link(code);
                                    }
                                    hbb_common::bail!("Device key rejected: {}", code);
                                }
                            }
                        }
                    }
                    "unlinked" => {
                        forget_dashboard_link("device has been permanently deleted");
                        hbb_common::bail!("Device unlinked from dashboard");
                    }
                    "settings:access_lock" => {
                        if let Some(data) = arr.get(1) {
                            let locked = data["locked"].as_bool() == Some(true);
                            apply_access_lock(locked);
                        }
                    }
                    "settings:easy_access" => {
                        if let Some(data) = arr.get(1) {
                            apply_easy_access(data["enabled"].as_bool() == Some(true));
                        }
                    }
                    "settings:desk" => {
                        if let Some(data) = arr.get(1) {
                            crate::desk::store(&data["desk"]);
                        }
                    }
                    "settings:session_notes" => {
                        if let Some(data) = arr.get(1) {
                            apply_session_notes(data["enabled"].as_bool() == Some(true));
                        }
                    }
                    "settings:device_name" => {
                        if let Some(data) = arr.get(1) {
                            if let Some(name) = data["name"].as_str() {
                                Config::set_option("dashboard_device_name".to_owned(), name.to_string());
                                if Config::get_option("pending-device-name") == name {
                                    Config::set_option("pending-device-name".to_owned(), String::new());
                                }
                            }
                        }
                    }
                    "settings:device_policy" => {
                        if let Some(data) = arr.get(1) {
                            apply_device_policy(data);
                        }
                    }
                    "settings:network" => {
                        if let Some(data) = arr.get(1) {
                            if let Some(event) = network_settings_from_event(data) {
                                queue_network_settings(event);
                            }
                        }
                    }
                    "ticket:reply" => {
                        if let Some(data) = arr.get(1) {
                            log::info!("Dashboard: ticket reply notification: {}", data);
                            TICKET_REPLY_COUNTER.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    "ticket:status" => {
                        if let Some(data) = arr.get(1) {
                            log::info!("Dashboard: ticket status notification: {}", data);
                            TICKET_STATUS_COUNTER.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    "wol:send" => {
                        if let Some(data) = arr.get(1) {
                            if let Some(target_mac) = data["target_mac"].as_str() {
                                log::info!("Dashboard: WoL request for MAC {}", target_mac);
                                #[cfg(not(target_os = "ios"))]
                                {
                                    if let Ok(mac_addr) = target_mac.parse() {
                                        let interfaces = default_net::get_interfaces();
                                        for iface in &interfaces {
                                            for ipv4 in &iface.ipv4 {
                                                log::info!("Sending WoL magic packet via {}", ipv4.addr);
                                                allow_err!(wol::send_wol(mac_addr, None, Some(std::net::IpAddr::V4(ipv4.addr))));
                                            }
                                        }
                                    } else {
                                        log::error!("Dashboard: invalid MAC address for WoL: {}", target_mac);
                                    }
                                }
                            }
                        }
                    }
                    "mcp:request" => {
                        if let Some(data) = arr.get(1) {
                            let request_id = data["request_id"].as_str().unwrap_or("").to_string();
                            if leaves_remote_commands_to_server() {
                                log::info!("Dashboard: MCP request (id={}) left to the service process", request_id);
                                return Ok(None);
                            }
                            let payload = &data["payload"];
                            let payload_str = payload.to_string();
                            log::info!("Dashboard: MCP request (id={})", request_id);
                            note_ai_activity(data);
                            #[cfg(target_os = "ios")]
                            {
                                let _ = payload_str;
                                return Ok(Some(("mcp:response".to_string(), serde_json::json!({
                                    "request_id": request_id,
                                    "response": {"error": "not supported on iOS"}
                                }))));
                            }
                            #[cfg(not(any(target_os = "android", target_os = "ios")))]
                            {
                                let mcp_resp = crate::mcp_server::handle_mcp_request(&payload_str)
                                    .unwrap_or_else(|| r#"{"error":"no response"}"#.to_string());
                                let resp_val: serde_json::Value = serde_json::from_str(&mcp_resp).unwrap_or_default();
                                return Ok(Some(("mcp:response".to_string(), serde_json::json!({
                                    "request_id": request_id,
                                    "response": resp_val
                                }))));
                            }
                            #[cfg(target_os = "android")]
                            {
                                let tx = _mcp_tx.clone();
                                std::thread::spawn(move || {
                                    let mcp_resp = crate::mcp_android::handle_mcp_request(&payload_str)
                                        .unwrap_or_else(|| r#"{"error":"no response"}"#.to_string());
                                    let resp_val: serde_json::Value = serde_json::from_str(&mcp_resp).unwrap_or_default();
                                    let _ = tx.send(("mcp:response".to_string(), serde_json::json!({
                                        "request_id": request_id,
                                        "response": resp_val
                                    })));
                                });
                                return Ok(None);
                            }
                        }
                    }
                    _ => {
                        log::debug!("Dashboard: unknown event '{}': {}", event, json_str);
                    }
                }
            }
        }
    }

    Ok(None)
}

fn get_timezone() -> String {
    #[cfg(target_os = "windows")]
    {
        if let Ok(tz) = std::env::var("TZ") {
            return tz;
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(tz) = std::env::var("TZ") {
            return tz;
        }
        if let Ok(tz) = std::fs::read_to_string("/etc/timezone") {
            let tz = tz.trim().to_string();
            if !tz.is_empty() {
                return tz;
            }
        }
    }
    let offset = chrono::Local::now().format("%:z").to_string();
    format!("UTC{}", offset)
}

pub fn set_in_session(active: bool, session_type: &str, remote_ip: &str, issue: &str, peer_name: &str) {
    if active {
        *SESSION_META.lock().unwrap() = (
            session_type.to_string(),
            remote_ip.to_string(),
            issue.chars().take(500).collect(),
            peer_name.chars().take(200).collect(),
        );
    }
    IN_SESSION.store(active, Ordering::Relaxed);
    SESSION_NOTIFY.notify_one();
}

pub fn submit_ticket(
    email: &str,
    subject: &str,
    description: &str,
    priority: &str,
) -> ResultType<(i64, i64)> {
    let device_id = Config::get_id();
    let dashboard_user_id = get_dashboard_user_id();
    let device_name = crate::common::hostname();
    let user_name = crate::username();

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let url = format!("{}?action=submitTicket", dashboard_api_base());
    let resp: serde_json::Value = client
        .post(&url)
        .form(&[
            ("device_id", device_id.as_str()),
            ("dashboard_user_id", dashboard_user_id.as_str()),
            ("device_name", device_name.as_str()),
            ("user_name", user_name.as_str()),
            ("user_email", email),
            ("subject", subject),
            ("description", description),
            ("priority", priority),
        ])
        .send()?
        .json()?;

    if resp["success"].as_bool() != Some(true) {
        hbb_common::bail!(
            "submitTicket failed: {}",
            resp["error"].as_str().unwrap_or("unknown error")
        );
    }
    let ticket_id = resp["ticket_id"].as_i64().unwrap_or(0);
    let ticket_number = resp["ticket_number"].as_i64().unwrap_or(ticket_id);
    Ok((ticket_id, ticket_number))
}

pub fn fetch_support_contact() -> ResultType<serde_json::Value> {
    let dashboard_user_id = get_dashboard_user_id();
    if dashboard_user_id.is_empty() {
        return Ok(serde_json::json!({}));
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let url = format!(
        "{}?action=getBranding&dashboard_user_id={}",
        dashboard_api_base(), dashboard_user_id
    );
    let resp: serde_json::Value = client.get(&url).send()?.json()?;
    let b = &resp["branding"];
    Ok(serde_json::json!({
        "name": b["company_name"].as_str().unwrap_or(""),
        "support_email": b["support_email"].as_str().unwrap_or(""),
        "phone": b["phone_number"].as_str().unwrap_or(""),
        "portal_slug": b["portal_slug"].as_str().unwrap_or(""),
    }))
}

pub fn get_my_tickets() -> ResultType<serde_json::Value> {
    let device_id = Config::get_id();
    let dashboard_user_id = get_dashboard_user_id();

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let url = format!("{}?action=getMyTickets", dashboard_api_base());
    let resp: serde_json::Value = client
        .post(&url)
        .form(&[
            ("device_id", device_id),
            ("dashboard_user_id", dashboard_user_id),
            ("device_key", get_device_key()),
        ])
        .send()?
        .json()?;
    if resp["success"].as_bool() != Some(true) {
        hbb_common::bail!(
            "getMyTickets failed: {}",
            resp["error"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(resp["tickets"].clone())
}

pub fn get_conversation(ticket_id: i64) -> ResultType<serde_json::Value> {
    let device_id = Config::get_id();

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let url = format!("{}?action=getCustomerConversation", dashboard_api_base());
    let resp: serde_json::Value = client
        .post(&url)
        .form(&[
            ("ticket_id", ticket_id.to_string()),
            ("device_id", device_id),
            ("device_key", get_device_key()),
        ])
        .send()?
        .json()?;
    if resp["success"].as_bool() != Some(true) {
        hbb_common::bail!(
            "getCustomerConversation failed: {}",
            resp["error"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(resp["messages"].clone())
}

pub fn get_attachments(ticket_id: i64) -> ResultType<serde_json::Value> {
    let device_id = Config::get_id();

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let url = format!("{}?action=getAttachments", dashboard_api_base());
    let resp: serde_json::Value = client
        .post(&url)
        .form(&[
            ("ticket_id", ticket_id.to_string()),
            ("device_id", device_id),
            ("device_key", get_device_key()),
        ])
        .send()?
        .json()?;
    if resp["success"].as_bool() != Some(true) {
        hbb_common::bail!(
            "getAttachments failed: {}",
            resp["error"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(resp["attachments"].clone())
}

pub fn add_reply(ticket_id: i64, message: &str) -> ResultType<bool> {
    let device_id = Config::get_id();

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let url = format!("{}?action=addCustomerReply", dashboard_api_base());
    let resp: serde_json::Value = client
        .post(&url)
        .form(&[
            ("ticket_id", ticket_id.to_string()),
            ("device_id", device_id),
            ("device_key", get_device_key()),
            ("message", message.to_string()),
        ])
        .send()?
        .json()?;
    if resp["success"].as_bool() != Some(true) {
        hbb_common::bail!(
            "addCustomerReply failed: {}",
            resp["error"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(resp["reopened"].as_bool().unwrap_or(false))
}

pub fn upload_attachment(ticket_id: i64, file_path: &str) -> ResultType<()> {
    let device_id = Config::get_id();
    let dashboard_user_id = get_dashboard_user_id();
    let file_path = percent_decode_path(file_path);
    log::info!("upload_attachment: ticket_id={}, file_path={}, device_id={}, dashboard_user_id={}",
        ticket_id, file_path, device_id, if dashboard_user_id.is_empty() { "(empty)" } else { &dashboard_user_id });
    let path = std::path::Path::new(&file_path);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let file_content = match std::fs::read(&file_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to read file '{}': {}", file_path, e);
            hbb_common::bail!("Cannot read file '{}': {}", file_path, e);
        }
    };
    log::info!("Read {} bytes from {}", file_content.len(), file_name);

    let encrypted = if !dashboard_user_id.is_empty() {
        encrypt_attachment(&file_content, &dashboard_user_id)
    } else {
        file_content
    };

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;
    let url = format!("{}?action=customerUploadAttachment", dashboard_api_base());

    let part = reqwest::blocking::multipart::Part::bytes(encrypted)
        .file_name(file_name.clone())
        .mime_str("application/octet-stream")?;

    let form = reqwest::blocking::multipart::Form::new()
        .text("action", "customerUploadAttachment")
        .text("ticket_id", ticket_id.to_string())
        .text("device_id", device_id)
        .text("device_key", get_device_key())
        .text("customer_name", "Customer")
        .part("file", part);

    log::info!("Uploading attachment to {}", url);
    let resp = client.post(&url).multipart(form).send()?;
    let status = resp.status();
    let body_text = resp.text().unwrap_or_default();
    log::info!("Upload response: HTTP {} body={}", status, body_text);
    let body: serde_json::Value = serde_json::from_str(&body_text).unwrap_or_default();
    if !status.is_success() {
        hbb_common::bail!("HTTP {}: {}", status, body.get("message").and_then(|m| m.as_str()).unwrap_or(&body_text));
    }
    if body["success"].as_bool() == Some(false) {
        hbb_common::bail!("{}", body["message"].as_str().unwrap_or("upload failed"));
    }
    log::info!("Attachment uploaded successfully: {}", file_name);
    Ok(())
}

pub fn get_ticket_reply_counter() -> u64 {
    TICKET_REPLY_COUNTER.load(Ordering::Relaxed)
}

pub fn get_ticket_status_counter() -> u64 {
    TICKET_STATUS_COUNTER.load(Ordering::Relaxed)
}

pub fn get_attachment_download_url(download_url: &str) -> String {
    if download_url.starts_with("http") {
        download_url.to_string()
    } else {
        format!("{}/{}", dashboard_api_base(), download_url)
    }
}

#[derive(Clone)]
pub struct AvatarIdentity {
    pub verified: bool,
    pub display_name: String,
    pub org_name: String,
    pub png: String,
}

static AVATAR_CACHE: Mutex<Vec<(String, std::time::Instant, AvatarIdentity)>> = Mutex::new(Vec::new());
const AVATAR_CACHE_TTL_SECS: u64 = 900;
const AVATAR_FETCH_TIMEOUT_SECS: u64 = 3;
const AVATAR_PNG_MAX_LEN: usize = 400_000;

fn session_note_outbox_path() -> std::path::PathBuf {
    Config::path("session-notes-outbox.jsonl")
}

fn canonical_tamper_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_uppercase()
}

pub fn tamper_code_hash(salt: &str, code: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(canonical_tamper_code(code).as_bytes());
    hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn verify_uninstall_code(code: &str) -> bool {
    let candidate = canonical_tamper_code(code);
    if candidate.is_empty() {
        return false;
    }
    let master_salt = hbb_common::config::tamper_master_salt();
    let master_hash = hbb_common::config::tamper_master_hash();
    if !master_hash.is_empty() && tamper_code_hash(&master_salt, &candidate) == master_hash {
        return true;
    }
    #[cfg(not(target_os = "ios"))]
    let service_options = crate::ipc::get_options();
    #[cfg(target_os = "ios")]
    let service_options = Config::get_options();
    let device_salt = service_options.get("tamper-code-salt").cloned().unwrap_or_default();
    let device_hash = service_options.get("tamper-code-hash").cloned().unwrap_or_default();
    !device_hash.is_empty() && tamper_code_hash(&device_salt, &candidate) == device_hash
}

fn tamper_attempt_outbox_path() -> std::path::PathBuf {
    Config::shared_path("tamper-attempt-outbox.jsonl")
}

pub fn queue_tamper_attempt(reason: &str) {
    let reason = match reason {
        "uninstall" | "service_stop" | "install_over" => reason,
        _ => "uninstall",
    };
    let line = serde_json::json!({ "reason": reason }).to_string();
    use std::io::Write;
    if std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(tamper_attempt_outbox_path())
        .and_then(|mut f| writeln!(f, "{}", line))
        .is_err()
    {
        #[cfg(windows)]
        if !crate::is_server() {
            crate::ipc::set_data(&crate::ipc::Data::TamperAttempt(reason.to_owned())).ok();
        }
    }
}

fn take_tamper_attempts() -> (String, Vec<String>) {
    let Ok(content) = std::fs::read_to_string(tamper_attempt_outbox_path()) else {
        return (String::new(), Vec::new());
    };
    let entries = content
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v["reason"].as_str().map(|s| s.to_string()))
        .collect();
    (content, entries)
}

fn clear_tamper_attempts(consumed: &str) {
    if let Ok(current) = std::fs::read_to_string(tamper_attempt_outbox_path()) {
        if current == consumed {
            let _ = std::fs::remove_file(tamper_attempt_outbox_path());
        }
    }
}

pub fn queue_session_note(note: String, billable: bool, peer_id: &str) {
    if get_dashboard_user_id().is_empty() {
        return;
    }
    let trimmed = note.trim().to_string();
    let noted_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let line = serde_json::json!({ "note": trimmed, "billable": billable, "noted_at": noted_at_ms, "peer_id": peer_id })
        .to_string();
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(session_note_outbox_path())
    {
        let _ = writeln!(f, "{}", line);
    }
}

pub fn queue_relayed_recording(r: RecordingFinished, technician: &str) {
    let valid_name = r.file_name.len() <= 300
        && r.file_name.starts_with("outgoing_")
        && (r.file_name.ends_with(".webm") || r.file_name.ends_with(".mp4"))
        && !r.file_name.contains(['/', '\\']);
    if !valid_name || get_dashboard_user_id().is_empty() {
        return;
    }
    let line = serde_json::json!({
        "direction": "outgoing",
        "file_name": r.file_name,
        "file_size": r.file_size,
        "duration_seconds": r.duration_seconds,
        "peer_id": Config::get_id(),
        "technician": technician,
        "session_start": r.session_start,
        "session_end": r.session_end,
    })
    .to_string();
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(scrap::record::recording_outbox_path())
    {
        let _ = writeln!(f, "{}", line);
    }
}

fn take_session_notes() -> (String, Vec<(String, bool, u64, String)>) {
    let Ok(content) = std::fs::read_to_string(session_note_outbox_path()) else {
        return (String::new(), Vec::new());
    };
    let entries = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .map(|v| (
            v["note"].as_str().unwrap_or("").to_string(),
            v["billable"].as_bool().unwrap_or(true),
            v["noted_at"].as_u64().unwrap_or(0),
            v["peer_id"].as_str().unwrap_or("").to_string(),
        ))
        .collect();
    (content, entries)
}

fn clear_session_notes(sent_content: &str) {
    if sent_content.is_empty() {
        return;
    }
    if std::fs::read_to_string(session_note_outbox_path())
        .map(|c| c == sent_content)
        .unwrap_or(false)
    {
        std::fs::remove_file(session_note_outbox_path()).ok();
    }
}

fn take_recording_outbox() -> (String, Vec<serde_json::Value>) {
    let path = scrap::record::recording_outbox_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return (String::new(), Vec::new());
    };
    let entries: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    (content, entries)
}

fn clear_recording_outbox(sent_content: &str) {
    if sent_content.is_empty() {
        return;
    }
    let path = scrap::record::recording_outbox_path();
    if std::fs::read_to_string(&path)
        .map(|c| c == sent_content)
        .unwrap_or(false)
    {
        std::fs::remove_file(&path).ok();
    }
}

pub fn purge_old_recordings() {
    loop {
        purge_recordings_once();
        std::thread::sleep(Duration::from_secs(12 * 3600));
    }
}

fn recording_retention_days() -> i64 {
    let forced = if is_linked() {
        Config::get_option(POLICY_RECORDING_RETENTION)
    } else {
        String::new()
    };
    let days = if forced.is_empty() {
        crate::ui_interface::get_option("recording-retention-days")
    } else {
        forced
    };
    days.parse().unwrap_or(0)
}

fn purge_recordings_once() {
    let days = recording_retention_days();
    if days <= 0 {
        return;
    }
    let cutoff = std::time::SystemTime::now() - Duration::from_secs(days as u64 * 86400);
    purge_recordings_in(&crate::ui_interface::video_save_directory(false), cutoff);
    #[cfg(windows)]
    if crate::platform::is_root() {
        purge_recordings_in(&crate::ui_interface::video_save_directory(true), cutoff);
    }
}

fn purge_recordings_in(dir: &str, cutoff: std::time::SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_recording = path
            .extension()
            .map(|e| e == "webm" || e == "mp4")
            .unwrap_or(false)
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("incoming_") || n.starts_with("outgoing_"))
                .unwrap_or(false);
        if !is_recording {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t < cutoff)
            .unwrap_or(false);
        if old {
            if let Err(e) = std::fs::remove_file(&path) {
                log::warn!("failed to purge old recording: {}", e);
            }
        }
    }
}

fn is_hex_token(s: &str, min: usize, max: usize) -> bool {
    s.len() >= min && s.len() <= max && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn clean_identity_text(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(64).collect::<String>().trim().to_string()
}

fn valid_avatar_png(png: &str) -> bool {
    let prefix = "data:image/png;base64,";
    if !png.starts_with(prefix) || png.len() > AVATAR_PNG_MAX_LEN {
        return false;
    }
    png[prefix.len()..]
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
}

fn unverified_identity() -> AvatarIdentity {
    AvatarIdentity {
        verified: false,
        display_name: String::new(),
        org_name: String::new(),
        png: String::new(),
    }
}

pub async fn mint_avatar_token() -> String {
    let secret = Config::get_option("avatar-mint-secret");
    if !is_hex_token(&secret, 32, 128) {
        return String::new();
    }
    let client = crate::common::make_http_client();
    let url = format!("{}?action=avatarMintToken", dashboard_api_base());
    let body = serde_json::json!({ "mint_secret": secret });
    let resp = tokio::time::timeout(
        Duration::from_secs(AVATAR_FETCH_TIMEOUT_SECS),
        client.post(&url).json(&body).send(),
    )
    .await;
    if let Ok(Ok(resp)) = resp {
        if let Ok(v) = resp.json::<serde_json::Value>().await {
            if v["success"].as_bool() == Some(true) {
                let token = v["avatar_token"].as_str().unwrap_or("").to_string();
                if is_hex_token(&token, 16, 64) {
                    return token;
                }
            }
        }
    }
    String::new()
}

pub async fn fetch_avatar_identity(token: &str) -> Option<AvatarIdentity> {
    if !is_hex_token(token, 16, 64) {
        return None;
    }
    {
        let cache = AVATAR_CACHE.lock().unwrap();
        if let Some((_, at, identity)) = cache.iter().find(|(t, _, _)| t == token) {
            if at.elapsed().as_secs() < AVATAR_CACHE_TTL_SECS {
                return Some(identity.clone());
            }
        }
    }
    let client = crate::common::make_http_client();
    let base = dashboard_api_base();
    let base = base.trim_end_matches('/').trim_end_matches("/api");
    let url = format!("{}/avatar/{}", base, token);
    let resp = tokio::time::timeout(
        Duration::from_secs(AVATAR_FETCH_TIMEOUT_SECS),
        client.get(&url).send(),
    )
    .await;
    let identity = match resp {
        Ok(Ok(r)) => {
            let status = r.status();
            if status.is_success() {
                match r.json::<serde_json::Value>().await {
                    Ok(v) if v["success"].as_bool() == Some(true) => {
                        let display_name = clean_identity_text(v["display_name"].as_str().unwrap_or(""));
                        let org_name = clean_identity_text(v["org_name"].as_str().unwrap_or(""));
                        let png = v["png"].as_str().unwrap_or("").to_string();
                        if display_name.is_empty() || !valid_avatar_png(&png) {
                            unverified_identity()
                        } else {
                            AvatarIdentity {
                                verified: true,
                                display_name,
                                org_name,
                                png,
                            }
                        }
                    }
                    _ => unverified_identity(),
                }
            } else if status.is_client_error() {
                unverified_identity()
            } else {
                return None;
            }
        }
        _ => return None,
    };
    let mut cache = AVATAR_CACHE.lock().unwrap();
    cache.retain(|(t, at, _)| t != token && at.elapsed().as_secs() < AVATAR_CACHE_TTL_SECS);
    cache.push((token.to_string(), std::time::Instant::now(), identity.clone()));
    Some(identity)
}
