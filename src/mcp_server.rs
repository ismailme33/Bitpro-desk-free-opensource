use std::io::{self, BufRead, Write};
use serde_json::{json, Value};
use std::time::Duration;
use std::time::Instant;
use std::collections::HashMap;
use std::sync::Mutex;
use hbb_common::tokio;
use hbb_common::config::PeerConfig;
use lazy_static::lazy_static;

const BIND_MAX_ATTEMPTS: u32 = 20;
const BIND_RETRY_SECS: u32 = 3;

struct WorkflowStep {
    tool_name: String,
    arguments: Value,
    timestamp_ms: u64,
}

struct WorkflowState {
    recording: bool,
    steps: Vec<WorkflowStep>,
    start_time: Option<Instant>,
}

lazy_static! {

    static ref DIFF_REFS: Mutex<HashMap<String, (Vec<u8>, u32, u32, Instant)>> =
        Mutex::new(HashMap::new());

    static ref FILE_TRANSFERS: Mutex<HashMap<String, FileTransferStatus>> =
        Mutex::new(HashMap::new());

    static ref WORKFLOW_STATE: Mutex<WorkflowState> =
        Mutex::new(WorkflowState {
            recording: false,
            steps: Vec::new(),
            start_time: None,
        });
}

const SERVER_NAME: &str = "hoptodesk-mcp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: &str = "2024-11-05";

fn parse_crop_region(args: &Value) -> Option<(i32, i32, i32, i32)> {
    Some((
        args["x"].as_i64()? as i32,
        args["y"].as_i64()? as i32,
        args["width"].as_i64()? as i32,
        args["height"].as_i64()? as i32,
    ))
}

pub fn run() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => {
                break;
            }
        };
        if let Some(resp) = process_line(&line) {
            if let Err(_) = writeln!(stdout, "{}", resp) {
                break;
            }
            let _ = stdout.flush();
        }
    }
}

pub fn run_tcp(port: u16) {
    use std::net::TcpListener;

    let addr = format!("127.0.0.1:{}", port);
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(_) => {
            return;
        }
    };

    let token = generate_auth_token();

    write_discovery_file(port, &token, "tcp");
    let _guard = McpFileGuard;

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => {
                continue;
            }
        };
        if stream.set_read_timeout(Some(MCP_AUTH_DEADLINE)).is_err() {
            continue;
        }
        let reader = io::BufReader::new(match stream.try_clone() {
            Ok(s) => s,
            Err(_) => continue,
        });
        let mut writer = stream;
        let mut authenticated = false;

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => {
                    break;
                }
            };

            if !authenticated {

                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if v["auth"].as_str() == Some(&token) {
                        authenticated = true;
                        let _ = writer.set_read_timeout(None);
                        let _ = writeln!(writer, r#"{{"authenticated":true}}"#);
                        let _ = writer.flush();
                        continue;
                    }
                }
                let _ = writeln!(writer, r#"{{"error":"authentication failed"}}"#);
                let _ = writer.flush();
                break;
            }

            if let Some(resp) = process_line(&line) {
                if let Err(_) = writeln!(writer, "{}", resp) {
                    break;
                }
                let _ = writer.flush();
            }
        }
    }
}

pub fn handle_mcp_request(json_str: &str) -> Option<String> {
    process_line(json_str)
}

fn process_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let request: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            return None;
        }
    };

    let method = request["method"].as_str().unwrap_or("");
    let id = request.get("id").cloned();

    if id.is_none() {
        return None;
    }

    let response = match method {
        "initialize" => handle_initialize(&request),
        "tools/list" => handle_tools_list(),
        "tools/call" => handle_tools_call(&request),
        "prompts/list" => handle_prompts_list(),
        "prompts/get" => handle_prompts_get(&request),
        "resources/list" => handle_resources_list(),
        "resources/read" => handle_resources_read(&request),
        "roots/list" => handle_roots_list(),
        "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("Unknown method: {}", method) }
        }),
    };

    let mut resp = response;
    if let Some(id_val) = id {
        resp["id"] = id_val;
    }

    Some(serde_json::to_string(&resp).unwrap_or_default())
}

#[tokio::main(flavor = "current_thread")]
pub async fn run_ws_local(port: u16) {
    use hbb_common::tokio::net::TcpListener;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let addr = format!("127.0.0.1:{}", port);
    let mut listener = None;
    let mut bind_error = String::new();
    for _ in 0..BIND_MAX_ATTEMPTS {
        match TcpListener::bind(&addr).await {
            Ok(l) => {
                listener = Some(l);
                break;
            }
            Err(e) => {
                bind_error = e.to_string();
                hbb_common::sleep(BIND_RETRY_SECS as f32).await;
            }
        }
    }
    let listener = match listener {
        Some(l) => l,
        None => {
            hbb_common::log::warn!("MCP local server did not start on {}: {}", addr, bind_error);
            remove_discovery_file();
            return;
        }
    };

    let token = generate_auth_token();

    write_discovery_file(port, &token, "websocket");
    let _guard = McpFileGuard;

    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => {
                continue;
            }
        };
        let auth_deadline = Instant::now() + MCP_AUTH_DEADLINE;
        let ws_stream = match tokio::time::timeout(
            MCP_AUTH_DEADLINE,
            tokio_tungstenite::accept_async(stream),
        )
        .await
        {
            Ok(Ok(ws)) => ws,
            _ => {
                continue;
            }
        };

        let (mut ws_sender, mut ws_receiver) = ws_stream.split();
        let mut authenticated = false;
        let token_clone = token.clone();

        loop {
            let next = if authenticated {
                ws_receiver.next().await
            } else {
                let remaining = auth_deadline.saturating_duration_since(Instant::now());
                match tokio::time::timeout(remaining, ws_receiver.next()).await {
                    Ok(next) => next,
                    Err(_) => break,
                }
            };
            let Some(msg) = next else {
                break;
            };
            let msg = match msg {
                Ok(m) => m,
                Err(_) => {
                    break;
                }
            };

            let text = match msg {
                WsMessage::Text(t) => t,
                WsMessage::Close(_) => break,
                WsMessage::Ping(d) => {
                    let _ = ws_sender.send(WsMessage::Pong(d)).await;
                    continue;
                }
                _ => continue,
            };

            if !authenticated {

                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    if v["auth"].as_str() == Some(&token_clone) {
                        authenticated = true;
                        let _ = ws_sender
                            .send(WsMessage::Text(r#"{"authenticated":true}"#.to_string()))
                            .await;
                        continue;
                    }
                }
                let _ = ws_sender
                    .send(WsMessage::Text(r#"{"error":"authentication failed"}"#.to_string()))
                    .await;
                let _ = ws_sender.close().await;
                break;
            }

            if let Some(resp) = process_line(&text) {
                if let Err(_) = ws_sender.send(WsMessage::Text(resp)).await {
                    break;
                }
            }
        }
    }
}

const MCP_AUTH_DEADLINE: Duration = Duration::from_secs(10);

fn generate_auth_token() -> String {
    use hbb_common::rand::Rng;
    let mut rng = hbb_common::rand::thread_rng();
    (0..32)
        .map(|_| {
            let idx: u8 = rng.gen_range(0..62);
            match idx {
                0..=9 => (b'0' + idx) as char,
                10..=35 => (b'a' + idx - 10) as char,
                _ => (b'A' + idx - 36) as char,
            }
        })
        .collect()
}

struct McpFileGuard;

fn remove_discovery_file() {
    let path = hbb_common::config::Config::path("").join("mcp.json");
    if discovery_file_is_ours_or_stale(&path) {
        let _ = std::fs::remove_file(&path);
    }
}

fn discovery_file_is_ours_or_stale(path: &std::path::Path) -> bool {
    use hbb_common::sysinfo::{Pid, ProcessRefreshKind, System};
    let Some(pid) = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["pid"].as_u64())
    else {
        return path.exists();
    };
    let pid = pid as u32;
    pid == std::process::id()
        || !System::new().refresh_process_specifics(Pid::from_u32(pid), ProcessRefreshKind::new())
}

impl Drop for McpFileGuard {
    fn drop(&mut self) {
        remove_discovery_file();
    }
}

fn write_discovery_file(port: u16, token: &str, transport: &str) {
    let url = match transport {
        "websocket" => format!("ws://127.0.0.1:{}", port),
        _ => format!("tcp://127.0.0.1:{}", port),
    };
    let discovery = json!({
        "mcp_version": PROTOCOL_VERSION,
        "server": SERVER_NAME,
        "version": SERVER_VERSION,
        "transport": transport,
        "url": url,
        "port": port,
        "token": token,
        "pid": std::process::id(),
    });
    let content = serde_json::to_string_pretty(&discovery).unwrap_or_default();

    let config_dir = hbb_common::config::Config::path("");
    if config_dir.exists() || std::fs::create_dir_all(&config_dir).is_ok() {
        let path = config_dir.join("mcp.json");
        if std::fs::write(&path, &content).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }

            let _ = writeln!(
                std::io::stderr(),
                "HopToDesk MCP {} on {} — discovery file: {}",
                transport,
                url,
                path.display()
            );
        }
    }
}

fn handle_initialize(_request: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "result": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {
                "tools": {},
                "prompts": {},
                "resources": {},
                "roots": { "listChanged": false }
            },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": SERVER_VERSION
            }
        }
    })
}

fn handle_prompts_list() -> Value {
    json!({
        "jsonrpc": "2.0",
        "result": {
            "prompts": [
                { "name": "device_health_check", "description": "Check device health: CPU, memory, disk, and connectivity status" },
                { "name": "automate_ui_task", "description": "Step-by-step UI automation using screenshot, find, click, and verify pattern" },
                { "name": "monitor_screen", "description": "Watch a screen region for changes and report when something happens" },
                { "name": "connect_and_verify", "description": "Connect to a remote peer and verify the session is working properly" },
                { "name": "diagnose_connection", "description": "Diagnose why a remote connection is failing or performing poorly" },
                { "name": "file_operations", "description": "Transfer and manage files between local and remote devices" },
                { "name": "run_maintenance", "description": "Run standard maintenance tasks: clear temp files, check updates, restart services" },
                { "name": "screen_interaction", "description": "Best practice pattern for screen interaction: screenshot → identify → click → verify" },
                { "name": "batch_device_check", "description": "Check status of multiple devices via dashboard API" }
            ]
        }
    })
}

fn handle_prompts_get(request: &Value) -> Value {
    let name = request["params"]["name"].as_str().unwrap_or("");
    let messages = match name {
        "device_health_check" => json!([{
            "role": "user",
            "content": { "type": "text", "text": "Check this device's health. Use get_device_info for basic status, then exec_operation with 'get_system_info' for CPU/memory, 'disk_usage' for storage, and 'network_info' for connectivity. Summarize any issues found." }
        }]),
        "automate_ui_task" => json!([{
            "role": "user",
            "content": { "type": "text", "text": "To automate a UI task: 1) Take a screenshot to understand the current state. 2) Identify the target element's coordinates. 3) Use mouse_click or type_text to interact. 4) Use verify_action_result to confirm the action worked. 5) Repeat for each step. Use wait_for_screen_change between steps to avoid acting too fast." }
        }]),
        "monitor_screen" => json!([{
            "role": "user",
            "content": { "type": "text", "text": "Monitor a screen region for changes. Use screen_diff_summary to capture a baseline, then periodically call it again with the reference_id to check for changes. Use wait_for_screen_change for event-driven monitoring with a specific region (x, y, width, height). Report what changed and where." }
        }]),
        "connect_and_verify" => json!([{
            "role": "user",
            "content": { "type": "text", "text": "Connect to a remote peer: 1) Use list_peers to find the target device. 2) Use connect_to_peer with the peer_id. 3) Use wait_for_event with 'connection_ready' to wait for video. 4) Use get_ui_state to verify toolbar, quality metrics, and permissions. 5) Take a screenshot to confirm the remote desktop is visible." }
        }]),
        "diagnose_connection" => json!([{
            "role": "user",
            "content": { "type": "text", "text": "Diagnose a connection issue: 1) Use get_device_info to check signal server status. 2) Use list_active_connections to see current sessions. 3) Use get_ui_state to check quality metrics (fps, delay, codec). 4) Use exec_operation with 'ping_test' to check network. 5) Use exec_operation with 'network_info' for interface details. Report findings and suggest fixes." }
        }]),
        "file_operations" => json!([{
            "role": "user",
            "content": { "type": "text", "text": "For file operations: 1) Use list_local_files to browse directories. 2) Use read_local_file to read file contents (max 1MB). 3) For remote file transfer, use connect_to_peer with connection_type 'file-transfer'. 4) Use get_clipboard/set_clipboard for small text transfers between machines." }
        }]),
        "run_maintenance" => json!([{
            "role": "user",
            "content": { "type": "text", "text": "Run maintenance tasks: 1) exec_operation 'get_system_info' to check current state. 2) exec_operation 'clear_temp_files' to free disk space. 3) exec_operation 'run_update_check' to check for updates. 4) exec_operation 'flush_dns' if having DNS issues. 5) exec_operation 'restart_hoptodesk' if the service needs a refresh. Report results of each step." }
        }]),
        "screen_interaction" => json!([{
            "role": "user",
            "content": { "type": "text", "text": "Best practice for screen interaction: 1) screenshot to see current state. 2) Identify target coordinates from the screenshot. 3) mouse_click or type_text to interact. 4) verify_action_result to confirm something changed. 5) screenshot again if needed. Use scroll to navigate long content. Use mouse_drag for selection or drag-and-drop. Use wait_for_screen_change to wait for animations/loading to complete before taking the next action." }
        }]),
        "batch_device_check" => json!([{
            "role": "user",
            "content": { "type": "text", "text": "To check multiple devices: 1) Use list_peers to get all known devices. 2) For each device, use connect_to_peer, wait_for_event 'connection_ready', then exec_operation 'get_system_info'. 3) disconnect_peer when done. 4) Compile a summary report of all device statuses. For dashboard-enrolled devices, the dashboard API can query device status without connecting." }
        }]),
        _ => {
            return json!({
                "jsonrpc": "2.0",
                "error": { "code": -32602, "message": format!("Unknown prompt: {}", name) }
            });
        }
    };
    json!({
        "jsonrpc": "2.0",
        "result": {
            "description": format!("Prompt: {}", name),
            "messages": messages
        }
    })
}

fn handle_resources_list() -> Value {
    json!({
        "jsonrpc": "2.0",
        "result": {
            "resources": [
                {
                    "uri": "hoptodesk://device/info",
                    "name": "Device Info",
                    "description": "Device ID, version, platform, hostname, and signal server status",
                    "mimeType": "application/json"
                },
                {
                    "uri": "hoptodesk://device/config",
                    "name": "Device Configuration",
                    "description": "HopToDesk configuration: device ID, dashboard enrollment, options",
                    "mimeType": "application/json"
                },
                {
                    "uri": "hoptodesk://device/peers",
                    "name": "Known Peers",
                    "description": "List of known peers from recent sessions, favorites, and LAN discovery",
                    "mimeType": "application/json"
                },
                {
                    "uri": "hoptodesk://device/connections",
                    "name": "Active Connections",
                    "description": "Currently active remote desktop connections",
                    "mimeType": "application/json"
                }
            ]
        }
    })
}

fn handle_resources_read(request: &Value) -> Value {
    let uri = request["params"]["uri"].as_str().unwrap_or("");
    let (content_text, mime) = match uri {
        "hoptodesk://device/info" => {
            let id = hbb_common::config::Config::get_id();
            let version = env!("CARGO_PKG_VERSION");
            let platform = std::env::consts::OS;
            let hostname = crate::common::hostname();
            let status_num = device_status_num();
            let status_text = match status_num {
                1 => "online",
                0 => "connecting",
                _ => "offline",
            };
            let info = json!({
                "device_id": id,
                "version": version,
                "platform": platform,
                "hostname": hostname,
                "status": status_text,
                "status_num": status_num
            });
            (serde_json::to_string_pretty(&info).unwrap_or_default(), "application/json")
        },
        "hoptodesk://device/config" => {
            let id = hbb_common::config::Config::get_id();
            let dashboard_user_id = hbb_common::config::Config::get_option("dashboard_user_id");
            let config_dir = hbb_common::config::Config::path("").to_string_lossy().to_string();
            let config = json!({
                "device_id": id,
                "dashboard_user_id": if dashboard_user_id.is_empty() { Value::Null } else { Value::String(dashboard_user_id) },
                "config_directory": config_dir,
                "version": env!("CARGO_PKG_VERSION"),
                "platform": std::env::consts::OS
            });
            (serde_json::to_string_pretty(&config).unwrap_or_default(), "application/json")
        },
        "hoptodesk://device/peers" => {
            let peers: Vec<Value> = PeerConfig::peers(None)
                .into_iter()
                .map(|(id, modified, config)| {
                    let last_seen = modified
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    json!({
                        "id": id,
                        "username": config.info.username,
                        "hostname": config.info.hostname,
                        "platform": config.info.platform,
                        "last_seen_epoch": last_seen
                    })
                })
                .collect();
            (serde_json::to_string_pretty(&peers).unwrap_or_default(), "application/json")
        },
        "hoptodesk://device/connections" => {
            match tool_list_active_connections() {
                Ok(content) => {
                    let text = content.as_array()
                        .and_then(|arr| arr.first())
                        .and_then(|c| c["text"].as_str())
                        .unwrap_or("[]")
                        .to_string();
                    (text, "application/json")
                },
                Err(e) => (format!("{{\"error\": \"{}\"}}", e), "application/json"),
            }
        },
        _ => {
            return json!({
                "jsonrpc": "2.0",
                "error": { "code": -32602, "message": format!("Unknown resource URI: {}", uri) }
            });
        }
    };
    json!({
        "jsonrpc": "2.0",
        "result": {
            "contents": [{
                "uri": uri,
                "mimeType": mime,
                "text": content_text
            }]
        }
    })
}

fn handle_roots_list() -> Value {
    let mut roots = Vec::new();

    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let path = std::path::PathBuf::from(home);
        roots.push(json!({
            "uri": format!("file://{}", path.to_string_lossy()),
            "name": "Home Directory"
        }));
    }

    let config_dir = hbb_common::config::Config::path("");
    roots.push(json!({
        "uri": format!("file://{}", config_dir.to_string_lossy()),
        "name": "HopToDesk Config"
    }));

    let tmp = std::env::temp_dir();
    roots.push(json!({
        "uri": format!("file://{}", tmp.to_string_lossy()),
        "name": "Temp Directory"
    }));

    json!({
        "jsonrpc": "2.0",
        "result": { "roots": roots }
    })
}

fn handle_tools_list() -> Value {
    json!({
        "jsonrpc": "2.0",
        "result": {
            "tools": [
                {
                    "name": "screenshot",
                    "description": "Capture one display (or a cropped region of it) and return as a PNG image. Pick the monitor with display; x/y/width/height crop relative to that display's top-left corner — useful for inspecting toolbar buttons, dialog text, or other small UI elements.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "display": {
                                "type": "integer",
                                "description": "Display index in the order the client lists monitors (0 = first). Default: 0"
                            },
                            "x": { "type": "integer", "description": "Crop region left X coordinate in pixels" },
                            "y": { "type": "integer", "description": "Crop region top Y coordinate in pixels" },
                            "width": { "type": "integer", "description": "Crop region width in pixels" },
                            "height": { "type": "integer", "description": "Crop region height in pixels" }
                        }
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "get_window_list",
                    "description": "List all visible windows with their titles, positions, and sizes",
                    "inputSchema": {
                        "type": "object",
                        "properties": {}
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "mouse_click",
                    "description": "Move the mouse to (x, y) and click",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "x": { "type": "integer", "description": "X coordinate in pixels" },
                            "y": { "type": "integer", "description": "Y coordinate in pixels" },
                            "button": {
                                "type": "string",
                                "enum": ["left", "right", "middle"],
                                "description": "Mouse button. Default: left"
                            },
                            "double_click": {
                                "type": "boolean",
                                "description": "Double-click instead of single click. Default: false"
                            }
                        },
                        "required": ["x", "y"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "type_text",
                    "description": "Type text using keyboard input. Supports unicode characters.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "text": { "type": "string", "description": "Text to type" }
                        },
                        "required": ["text"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "key_press",
                    "description": "Press a keyboard key or combination (e.g. 'Return', 'ctrl+a', 'alt+F4')",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "key": { "type": "string", "description": "Key name (e.g. 'Return', 'Escape', 'Tab', 'BackSpace', 'Delete', 'F1'-'F12', 'Up', 'Down', 'Left', 'Right', 'Home', 'End', 'PageUp', 'PageDown')" },
                            "modifiers": {
                                "type": "array",
                                "items": { "type": "string", "enum": ["ctrl", "alt", "shift", "meta"] },
                                "description": "Modifier keys to hold while pressing"
                            }
                        },
                        "required": ["key"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "mouse_move",
                    "description": "Move the mouse to (x, y) without clicking",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "x": { "type": "integer", "description": "X coordinate in pixels" },
                            "y": { "type": "integer", "description": "Y coordinate in pixels" }
                        },
                        "required": ["x", "y"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "get_device_info",
                    "description": "Get this device's ID, version, platform, hostname, and signal server connection status",
                    "inputSchema": { "type": "object", "properties": {} },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "list_peers",
                    "description": "List known peers (recent sessions, favorites, LAN discovered). Returns peer ID, username, hostname, platform.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "filter": {
                                "type": "string",
                                "enum": ["all", "recent", "favorites", "lan"],
                                "description": "Which peers to list. Default: all"
                            }
                        }
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "connect_to_peer",
                    "description": "Initiate a remote desktop connection to a peer by device ID. Spawns a new connection window. Optionally pass a password to skip the password dialog.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "peer_id": { "type": "string", "description": "The peer device ID to connect to" },
                            "connection_type": {
                                "type": "string",
                                "enum": ["connect", "file-transfer", "port-forward"],
                                "description": "Type of connection. Default: connect"
                            },
                            "password": {
                                "type": "string",
                                "description": "Optional password for the remote peer. If provided, skips the password dialog."
                            }
                        },
                        "required": ["peer_id"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "disconnect_peer",
                    "description": "Close the remote connection to a peer by device ID",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "peer_id": { "type": "string", "description": "The peer device ID to disconnect" }
                        },
                        "required": ["peer_id"]
                    },
                    "annotations": { "readOnlyHint": false, "destructiveHint": true }
                },
                {
                    "name": "list_active_connections",
                    "description": "List all currently active remote connections with their type and alive status",
                    "inputSchema": { "type": "object", "properties": {} },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "get_ui_state",
                    "description": "Get rich structured UI state for HopToDesk. Returns per-connection: toolbar buttons, connection quality (speed/fps/delay/codec), display info (resolution, current monitor), permissions (keyboard/clipboard/audio/file), dialog/popup state, video dimensions, view settings, recording/privacy mode. No screenshot needed — use this to assess UI health programmatically.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "peer_id": {
                                "type": "string",
                                "description": "Optional: only return UI state for this specific connection. Default: return all."
                            }
                        }
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "dismiss_dialog",
                    "description": "Dismiss the currently visible modal dialog on a connection window (e.g. security code prompt, password prompt). Sends a signal to the child process via stdin pipe.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "peer_id": {
                                "type": "string",
                                "description": "The peer ID of the connection whose dialog to dismiss. Use 'all' to dismiss dialogs on all connections."
                            }
                        },
                        "required": ["peer_id"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "click_toolbar_button",
                    "description": "Click a toolbar button or submenu item on a remote connection window. Supports top-level buttons and submenu items using 'menu:item' format. Use get_ui_state to see available buttons and submenu items.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "peer_id": {
                                "type": "string",
                                "description": "The peer ID of the connection whose toolbar button to click."
                            },
                            "button_id": {
                                "type": "string",
                                "description": "Button ID or 'menu:item' for submenu. Top-level: fullscreen, chat, transfer-file, remote-print, recording, privacy-mode. Submenus: 'action:ctrl-alt-del', 'action:lock-screen', 'action:restart_remote_device', 'action:take-screenshot', 'action:refresh', 'action:block-input', 'action:tunnel', 'display:original', 'display:shrink', 'display:stretch', 'display:best', 'display:balanced', 'display:low', 'display:view-only', 'display:show-remote-cursor', 'display:disable-audio', 'display:disable-clipboard', 'display:lock-after-session-end', 'keyboard:legacy', 'keyboard:map', 'keyboard:translate'"
                            }
                        },
                        "required": ["peer_id", "button_id"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "transfer_file",
                    "description": "Transfer one file or folder over an open file-transfer window. Open the window first with connect_to_peer using connection_type 'file-transfer'. Use direction 'download' to pull from the remote machine, 'upload' to push from this machine. Returns once the job is queued; poll get_ui_state or the transfer window for progress.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "peer_id": { "type": "string", "description": "Peer ID with an open file-transfer window." },
                            "direction": { "type": "string", "enum": ["download", "upload"], "description": "download pulls from the remote machine, upload pushes from this one. Defaults to download." },
                            "path": { "type": "string", "description": "Full source path of the file or folder." },
                            "to": { "type": "string", "description": "Full destination path, including the file or folder name." }
                        },
                        "required": ["peer_id", "path", "to"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "open_tunnel",
                    "description": "Add a TCP tunnel on an open port-forward window, forwarding a local port to a host and port reachable from the remote machine. Open the window first with connect_to_peer using connection_type 'port-forward'. Fails if the remote machine's policy or settings disable tunneling.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "peer_id": { "type": "string", "description": "Peer ID with an open port-forward window." },
                            "local_port": { "type": "integer", "description": "Port to listen on locally, 1 to 65535." },
                            "remote_host": { "type": "string", "description": "Host to reach from the remote machine. Defaults to localhost." },
                            "remote_port": { "type": "integer", "description": "Port on the remote host, 1 to 65535." }
                        },
                        "required": ["peer_id", "local_port", "remote_port"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "test_print",
                    "description": "Send a test print job to the configured remote printer. This simulates receiving a print job from the remote machine — useful for verifying the local print pipeline works. Requires set_remote_printer to be configured first.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "text": {
                                "type": "string",
                                "description": "Text content to print. Default: 'HopToDesk Remote Print Test'"
                            }
                        }
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "set_remote_printer",
                    "description": "Set the printer for remote print jobs and enable auto-print mode. When enabled, print jobs from the remote machine are automatically sent to the selected printer without showing a dialog. Use 'auto' for printer_name to auto-select the only available printer. Returns the list of available printers if no name is given.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "printer_name": {
                                "type": "string",
                                "description": "Printer name to use, or 'auto' to select the only available printer. Omit to list available printers."
                            },
                            "auto_print": {
                                "type": "boolean",
                                "description": "Enable auto-print mode (default: true). When true, incoming print jobs are sent directly to the printer without prompting."
                            }
                        }
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "wait_for_event",
                    "description": "Block until a specified event occurs or timeout. Eliminates polling — use this instead of looping get_ui_state. Events: 'connection_ready' (peer connects and video starts), 'connection_closed' (peer disconnects), 'dialog_appeared' (dialog/prompt shows up), 'dialog_dismissed' (dialog closes), 'quality_stable' (fps > 0 and delay < 500ms). Returns the current state when the event fires.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "event": {
                                "type": "string",
                                "enum": ["connection_ready", "connection_closed", "dialog_appeared", "dialog_dismissed", "quality_stable"],
                                "description": "The event to wait for"
                            },
                            "peer_id": {
                                "type": "string",
                                "description": "Peer ID to monitor. Required for connection_ready, connection_closed, dialog_appeared, dialog_dismissed, quality_stable."
                            },
                            "timeout_ms": {
                                "type": "integer",
                                "description": "Maximum time to wait in milliseconds. Default: 30000 (30s). Max: 60000 (60s)."
                            }
                        },
                        "required": ["event", "peer_id"]
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "exec_operation",
                    "description": "Execute a predefined system operation on this device. Operations are hardcoded — only the operation name is accepted, no raw shell commands. Device must be enrolled with a dashboard.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "operation": {
                                "type": "string",
                                "enum": ["get_system_info", "disk_usage", "list_processes", "network_info", "get_service_logs", "ping_test", "installed_software", "get_security_posture", "restart_hoptodesk", "kill_process", "flush_dns", "reboot_device", "shutdown_device", "clear_temp_files", "run_update_check", "update_client", "list_services", "get_event_logs", "restart_service"],
                                "description": "The operation to execute"
                            },
                            "sort_by": { "type": "string", "enum": ["cpu", "memory"], "description": "For list_processes: sort by cpu or memory" },
                            "limit": { "type": "integer", "description": "For list_processes: max number of processes" },
                            "lines": { "type": "integer", "description": "For get_service_logs: number of log lines (max 200)" },
                            "host": { "type": "string", "description": "For ping_test: hostname or IP to ping" },
                            "count": { "type": "integer", "description": "For ping_test: number of pings (max 10). For get_event_logs: number of events (max 200)" },
                            "log": { "type": "string", "enum": ["System", "Application"], "description": "For get_event_logs: which log to read" },
                            "level": { "type": "string", "enum": ["error", "warning", "all"], "description": "For get_event_logs: error, warning and worse, or all events" },
                            "process_name": { "type": "string", "description": "For kill_process: name of process to kill" },
                            "pid": { "type": "integer", "description": "For kill_process: PID to kill" }
                        },
                        "required": ["operation"]
                    },
                    "annotations": { "readOnlyHint": false, "destructiveHint": true, "confirmationHint": true }
                },
                {
                    "name": "get_clipboard",
                    "description": "Read the current text content of the local clipboard",
                    "inputSchema": { "type": "object", "properties": {} },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "set_clipboard",
                    "description": "Set the local clipboard to the specified text content",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "text": { "type": "string", "description": "Text to copy to the clipboard" }
                        },
                        "required": ["text"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "send_chat_message",
                    "description": "Send a chat message to a connected remote peer. Requires an active remote desktop connection to the peer.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "peer_id": { "type": "string", "description": "The peer ID of the active connection to send the message to" },
                            "text": { "type": "string", "description": "The chat message text to send" }
                        },
                        "required": ["peer_id", "text"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "switch_permission",
                    "description": "Toggle a permission on an incoming remote connection. Only works for connections where a remote peer is connected TO this device (not outgoing connections you initiated). Permissions: keyboard, clipboard, audio, file, restart, recording.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "conn_id": { "type": "integer", "description": "The incoming connection ID (from list_incoming_connections)" },
                            "permission": {
                                "type": "string",
                                "enum": ["keyboard", "clipboard", "audio", "file", "restart", "recording"],
                                "description": "The permission to toggle"
                            },
                            "enabled": { "type": "boolean", "description": "true to enable, false to disable" }
                        },
                        "required": ["conn_id", "permission", "enabled"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "list_incoming_connections",
                    "description": "List all incoming remote connections (peers connected TO this device). Returns connection ID, peer info, authorization status, and permissions. Use conn_id with switch_permission to manage permissions.",
                    "inputSchema": { "type": "object", "properties": {} },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "list_local_files",
                    "description": "List files and directories at a given path on this device. Returns name, size, type (file/directory), and modified timestamp.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "Directory path to list. Default: home directory" },
                            "include_hidden": { "type": "boolean", "description": "Include hidden files (starting with '.'). Default: false" }
                        }
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "read_local_file",
                    "description": "Read the text content of a local file. For binary files, returns base64-encoded content. Max 1MB.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "Absolute path to the file to read" }
                        },
                        "required": ["path"]
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "upload_file_to_url",
                    "description": "Upload a local file to a dashboard-issued https URL (used by the dashboard's pull_file_from_device flow). Returns immediately with a transfer id; the upload runs in the background. Poll get_file_transfer_status with the transfer_id.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "Absolute path of the local file to upload" },
                            "url": { "type": "string", "description": "The https destination URL to PUT the file to" },
                            "transfer_id": { "type": "string", "description": "Transfer id to track this upload under" },
                            "max_bytes": { "type": "integer", "description": "Reject files larger than this many bytes" }
                        },
                        "required": ["path", "url", "transfer_id"]
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "download_file_from_url",
                    "description": "Download a file from an https URL to a local path (used by the dashboard's push_file_to_device flow). Returns immediately with a transfer id; the download runs in the background. Poll get_file_transfer_status with the transfer_id.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "url": { "type": "string", "description": "The https URL to download from" },
                            "to": { "type": "string", "description": "Absolute destination path on this device, including the file name" },
                            "transfer_id": { "type": "string", "description": "Transfer id to track this download under" },
                            "overwrite": { "type": "boolean", "description": "Overwrite an existing destination file. Default false" },
                            "max_bytes": { "type": "integer", "description": "Reject downloads larger than this many bytes" }
                        },
                        "required": ["url", "to", "transfer_id"]
                    },
                    "annotations": { "readOnlyHint": false, "destructiveHint": true }
                },
                {
                    "name": "get_file_transfer_status",
                    "description": "Get the status of an upload_file_to_url or download_file_from_url transfer by its transfer_id. Returns status (pending, complete, or failed), bytes transferred, and any error.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "transfer_id": { "type": "string", "description": "The transfer id returned by the transfer tool" }
                        },
                        "required": ["transfer_id"]
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "run_command",
                    "description": "Execute a shell command on this device. Runs via cmd.exe on Windows or /bin/sh on Unix. Device must be enrolled with a dashboard. Output is truncated to 64KB.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "command": { "type": "string", "description": "The shell command to execute" },
                            "timeout": { "type": "integer", "description": "Timeout in seconds (default: 30, max: 120)" }
                        },
                        "required": ["command"]
                    },
                    "annotations": { "readOnlyHint": false, "destructiveHint": true, "confirmationHint": true }
                },
                {
                    "name": "scroll",
                    "description": "Scroll the mouse wheel at a position. Move the mouse to (x, y) first if provided, then scroll in the given direction.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "x": { "type": "integer", "description": "X coordinate to scroll at. Omit to scroll at current position." },
                            "y": { "type": "integer", "description": "Y coordinate to scroll at. Omit to scroll at current position." },
                            "direction": {
                                "type": "string",
                                "enum": ["up", "down", "left", "right"],
                                "description": "Scroll direction. Default: down"
                            },
                            "amount": { "type": "integer", "description": "Number of scroll lines. Default: 3" }
                        }
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "mouse_drag",
                    "description": "Click and drag from one point to another. Useful for selecting text, moving windows, resizing, or drag-and-drop operations.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "start_x": { "type": "integer", "description": "Start X coordinate" },
                            "start_y": { "type": "integer", "description": "Start Y coordinate" },
                            "end_x": { "type": "integer", "description": "End X coordinate" },
                            "end_y": { "type": "integer", "description": "End Y coordinate" },
                            "button": {
                                "type": "string",
                                "enum": ["left", "right", "middle"],
                                "description": "Mouse button. Default: left"
                            },
                            "steps": { "type": "integer", "description": "Number of intermediate move steps for smooth drag. Default: 10" }
                        },
                        "required": ["start_x", "start_y", "end_x", "end_y"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "wait_for_screen_change",
                    "description": "Block until pixels change on the screen (or in a region). Use after performing an action to wait for the UI to update instead of polling with screenshots. Returns when the screen changes or timeout is reached.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "x": { "type": "integer", "description": "Region left X coordinate" },
                            "y": { "type": "integer", "description": "Region top Y coordinate" },
                            "width": { "type": "integer", "description": "Region width" },
                            "height": { "type": "integer", "description": "Region height" },
                            "timeout_ms": { "type": "integer", "description": "Max wait time in milliseconds. Default: 10000. Max: 60000." },
                            "threshold": { "type": "number", "description": "Percentage of pixels that must change to trigger. Default: 1.0" }
                        }
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "screen_diff_summary",
                    "description": "Two-phase screen comparison. Call once with no reference_id to capture a baseline (returns a reference_id). Then perform your action. Then call again with the reference_id to compare — returns which grid sectors changed and by how much. References expire after 60 seconds.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "reference_id": { "type": "string", "description": "Reference ID from a previous baseline capture. Omit to capture a new baseline." },
                            "x": { "type": "integer", "description": "Region left X coordinate" },
                            "y": { "type": "integer", "description": "Region top Y coordinate" },
                            "width": { "type": "integer", "description": "Region width" },
                            "height": { "type": "integer", "description": "Region height" }
                        }
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "verify_action_result",
                    "description": "Verify that a screen action had an effect. Captures the current screen, then polls for pixel changes. Returns whether the screen changed and which regions were affected. Use immediately after clicking a button or performing an action.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "x": { "type": "integer", "description": "Region left X coordinate" },
                            "y": { "type": "integer", "description": "Region top Y coordinate" },
                            "width": { "type": "integer", "description": "Region width" },
                            "height": { "type": "integer", "description": "Region height" },
                            "timeout_ms": { "type": "integer", "description": "Max wait time in milliseconds. Default: 3000. Max: 30000." },
                            "threshold": { "type": "number", "description": "Percentage of pixels that must change. Default: 1.0" }
                        }
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "start_recording",
                    "description": "Start recording MCP tool calls into a workflow. All subsequent tool calls (except recording tools) will be captured with their arguments and relative timestamps. Call stop_recording to get the recorded workflow.",
                    "inputSchema": { "type": "object", "properties": {} },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "stop_recording",
                    "description": "Stop recording and return the captured workflow as a JSON array of steps. Each step has tool_name, arguments, and delay_ms (time since previous step). The returned workflow can be passed to replay_workflow.",
                    "inputSchema": { "type": "object", "properties": {} },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "replay_workflow",
                    "description": "Replay a previously recorded workflow. Executes each step in sequence with the recorded delays. Use delay_multiplier to speed up (0.5) or slow down (2.0) playback.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "steps": {
                                "type": "array",
                                "description": "Array of workflow steps from stop_recording. Each: {tool_name, arguments, delay_ms}",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "tool_name": { "type": "string" },
                                        "arguments": { "type": "object" },
                                        "delay_ms": { "type": "integer" }
                                    },
                                    "required": ["tool_name", "arguments"]
                                }
                            },
                            "delay_multiplier": { "type": "number", "description": "Multiply all delays by this factor. Default: 1.0. Use 0 for no delays." }
                        },
                        "required": ["steps"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "read_screen_text",
                    "description": "OCR the primary display (or a cropped region) and return recognized text. Uses native OS OCR (Windows Media.Ocr / macOS Vision framework). Useful for reading dialog text, error messages, labels, or any on-screen text.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "x": { "type": "integer", "description": "Crop region left X coordinate in pixels" },
                            "y": { "type": "integer", "description": "Crop region top Y coordinate in pixels" },
                            "width": { "type": "integer", "description": "Crop region width in pixels" },
                            "height": { "type": "integer", "description": "Crop region height in pixels" },
                            "language": { "type": "string", "description": "BCP-47 language tag (e.g. 'en', 'ja'). Default: system default" }
                        }
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "get_screen_size",
                    "description": "Get the primary display resolution in pixels. Returns width and height.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {}
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "find_element",
                    "description": "OCR the screen and find text matching a query. Returns matching text blocks with their pixel coordinates and center points, useful for clicking. Case-insensitive substring match by default.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "text": { "type": "string", "description": "Text to search for on screen" },
                            "exact": { "type": "boolean", "description": "Require exact match instead of substring. Default: false" }
                        },
                        "required": ["text"]
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "click_text",
                    "description": "OCR the screen, find the specified text, and click its center in one step. Reduces 3 round-trips (screenshot + find + click) to 1.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "text": { "type": "string", "description": "Text to find and click" },
                            "button": { "type": "string", "enum": ["left", "right", "middle"], "description": "Mouse button. Default: left" },
                            "occurrence": { "type": "integer", "description": "Which occurrence to click if text appears multiple times (1-based). Default: 1" }
                        },
                        "required": ["text"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "wait_for_text",
                    "description": "Block until specific text appears on screen via OCR polling. Returns when text is found or timeout is reached.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "text": { "type": "string", "description": "Text to wait for (case-insensitive substring match)" },
                            "timeout_ms": { "type": "integer", "description": "Maximum wait time in ms. Default: 10000, max: 60000" },
                            "interval_ms": { "type": "integer", "description": "Polling interval in ms. Default: 500" }
                        },
                        "required": ["text"]
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "key_hold",
                    "description": "Press and hold a key without releasing it. Use with key_release for modifier+click workflows (e.g. Ctrl+click, Shift+drag).",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "key": { "type": "string", "description": "Key to hold (e.g. 'ctrl', 'shift', 'alt', 'meta', or any key name from key_press)" }
                        },
                        "required": ["key"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "key_release",
                    "description": "Release a previously held key. Must be paired with a prior key_hold call.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "key": { "type": "string", "description": "Key to release (must match a prior key_hold)" }
                        },
                        "required": ["key"]
                    },
                    "annotations": { "readOnlyHint": false }
                }
            ]
        }
    })
}

fn handle_tools_call(request: &Value) -> Value {
    let tool_name = request["params"]["name"].as_str().unwrap_or("");
    let arguments = &request["params"]["arguments"];

    let result = match tool_name {
        "screenshot" => tool_screenshot(arguments),
        "get_window_list" => tool_get_window_list(),
        "mouse_click" => tool_mouse_click(arguments),
        "type_text" => tool_type_text(arguments),
        "key_press" => tool_key_press(arguments),
        "mouse_move" => tool_mouse_move(arguments),
        "get_device_info" => tool_get_device_info(),
        "list_peers" => tool_list_peers(arguments),
        "connect_to_peer" => tool_connect_to_peer(arguments),
        "disconnect_peer" => tool_disconnect_peer(arguments),
        "list_active_connections" => tool_list_active_connections(),
        "get_ui_state" => tool_get_ui_state(arguments),
        "dismiss_dialog" => tool_dismiss_dialog(arguments),
        "click_toolbar_button" => tool_click_toolbar_button(arguments),
        "transfer_file" => tool_transfer_file(arguments),
        "open_tunnel" => tool_open_tunnel(arguments),
        "set_remote_printer" => tool_set_remote_printer(arguments),
        "test_print" => tool_test_print(arguments),
        "wait_for_event" => tool_wait_for_event(arguments),
        "exec_operation" => tool_exec_operation(arguments),
        "get_clipboard" => tool_get_clipboard(),
        "set_clipboard" => tool_set_clipboard(arguments),
        "send_chat_message" => tool_send_chat_message(arguments),
        "switch_permission" => tool_switch_permission(arguments),
        "list_incoming_connections" => tool_list_incoming_connections(),
        "list_local_files" => tool_list_local_files(arguments),
        "read_local_file" => tool_read_local_file(arguments),
        "upload_file_to_url" => tool_upload_file_to_url(arguments),
        "download_file_from_url" => tool_download_file_from_url(arguments),
        "get_file_transfer_status" => tool_get_file_transfer_status(arguments),
        "run_command" => tool_run_command(arguments),
        "scroll" => tool_scroll(arguments),
        "mouse_drag" => tool_mouse_drag(arguments),
        "wait_for_screen_change" => tool_wait_for_screen_change(arguments),
        "screen_diff_summary" => tool_screen_diff_summary(arguments),
        "verify_action_result" => tool_verify_action_result(arguments),
        "start_recording" => tool_start_recording(),
        "stop_recording" => tool_stop_recording(),
        "replay_workflow" => tool_replay_workflow(arguments),
        "read_screen_text" => tool_read_screen_text(arguments),
        "get_screen_size" => tool_get_screen_size(),
        "find_element" => tool_find_element(arguments),
        "click_text" => tool_click_text(arguments),
        "wait_for_text" => tool_wait_for_text(arguments),
        "key_hold" => tool_key_hold(arguments),
        "key_release" => tool_key_release(arguments),
        "get_device_health" => tool_get_device_health(),
        "android_global_action" => tool_android_global_action(arguments),
        "android_tap" => tool_android_tap(arguments),
        _ => Err(format!("Unknown tool: {}", tool_name)),
    };

    if !matches!(tool_name, "start_recording" | "stop_recording" | "replay_workflow") {
        if let Ok(mut state) = WORKFLOW_STATE.lock() {
            if state.recording {
                let elapsed = state.start_time
                    .map(|t| t.elapsed().as_millis() as u64)
                    .unwrap_or(0);
                state.steps.push(WorkflowStep {
                    tool_name: tool_name.to_string(),
                    arguments: arguments.clone(),
                    timestamp_ms: elapsed,
                });
            }
        }
    }

    match result {
        Ok(content) => json!({
            "jsonrpc": "2.0",
            "result": { "content": content }
        }),
        Err(msg) => json!({
            "jsonrpc": "2.0",
            "result": {
                "content": [{ "type": "text", "text": format!("Error: {}", msg) }],
                "isError": true
            }
        }),
    }
}

fn tool_screenshot(args: &Value) -> Result<Value, String> {

    #[cfg(windows)]
    {
        if let Ok(result) = screenshot_gdi(args) {
            return Ok(result);
        }
    }
    screenshot_scrap(args)
}

#[cfg(windows)]
fn screenshot_gdi(args: &Value) -> Result<Value, String> {
    use hbb_common::base64::Engine;

    #[allow(clashing_extern_declarations)]
    extern "system" {
        fn GetDC(hwnd: isize) -> isize;
        fn ReleaseDC(hwnd: isize, hdc: isize) -> i32;
        fn CreateCompatibleDC(hdc: isize) -> isize;
        fn CreateCompatibleBitmap(hdc: isize, w: i32, h: i32) -> isize;
        fn SelectObject(hdc: isize, obj: isize) -> isize;
        fn BitBlt(dst: isize, x: i32, y: i32, w: i32, h: i32, src: isize, sx: i32, sy: i32, rop: u32) -> i32;
        fn DeleteDC(hdc: isize) -> i32;
        fn DeleteObject(obj: isize) -> i32;
        fn GetSystemMetrics(idx: i32) -> i32;
        fn GetDIBits(hdc: isize, hbm: isize, start: u32, lines: u32, bits: *mut u8, bi: *mut u8, usage: u32) -> i32;
    }

    const SM_CXSCREEN: i32 = 0;
    const SM_CYSCREEN: i32 = 1;
    const SRCCOPY: u32 = 0x00CC0020;

    let display_idx = args["display"].as_u64().unwrap_or(0) as usize;
    unsafe {
        let (origin_x, origin_y, screen_w, screen_h) = match gdi_display_rect(display_idx) {
            Some(rect) => rect,
            None if display_idx == 0 => (
                0,
                0,
                GetSystemMetrics(SM_CXSCREEN),
                GetSystemMetrics(SM_CYSCREEN),
            ),
            None => return Err(format!("Display {} not found", display_idx)),
        };
        if screen_w <= 0 || screen_h <= 0 {
            return Err("No display available".to_string());
        }

        let (src_x, src_y, cap_w, cap_h) = if let Some((cx, cy, cw, ch)) = parse_crop_region(args) {
            let cx = cx.max(0).min(screen_w);
            let cy = cy.max(0).min(screen_h);
            let cw = cw.min(screen_w - cx).max(1);
            let ch = ch.min(screen_h - cy).max(1);
            (origin_x + cx, origin_y + cy, cw, ch)
        } else {
            (origin_x, origin_y, screen_w, screen_h)
        };

        let screen_dc = GetDC(0);
        if screen_dc == 0 {
            return Err("GetDC failed".to_string());
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        let bitmap = CreateCompatibleBitmap(screen_dc, cap_w, cap_h);
        let old = SelectObject(mem_dc, bitmap);
        BitBlt(mem_dc, 0, 0, cap_w, cap_h, screen_dc, src_x, src_y, SRCCOPY);

        let mut bmi = [0u8; 44];
        bmi[0] = 40;
        bmi[4..8].copy_from_slice(&cap_w.to_le_bytes());
        let neg_h = (-cap_h).to_le_bytes();
        bmi[8..12].copy_from_slice(&neg_h);
        bmi[12..14].copy_from_slice(&1u16.to_le_bytes());
        bmi[14..16].copy_from_slice(&32u16.to_le_bytes());

        let mut pixels = vec![0u8; (cap_w * cap_h * 4) as usize];
        let ret = GetDIBits(mem_dc, bitmap, 0, cap_h as u32, pixels.as_mut_ptr(), bmi.as_mut_ptr(), 0);

        SelectObject(mem_dc, old);
        DeleteObject(bitmap);
        DeleteDC(mem_dc);
        ReleaseDC(0, screen_dc);

        if ret == 0 {
            return Err("GetDIBits failed".to_string());
        }

        let mut rgba = Vec::with_capacity(pixels.len());
        for chunk in pixels.chunks(4) {
            rgba.push(chunk[2]);
            rgba.push(chunk[1]);
            rgba.push(chunk[0]);
            rgba.push(255);
        }

        let mut png = Vec::new();
        repng::encode(&mut png, cap_w as _, cap_h as _, &rgba)
            .map_err(|e| format!("PNG encode error: {}", e))?;

        let b64 = hbb_common::base64::engine::general_purpose::STANDARD.encode(&png);

        Ok(json!([{
            "type": "image",
            "data": b64,
            "mimeType": "image/png"
        }]))
    }
}

#[cfg(windows)]
fn gdi_display_rect(display_idx: usize) -> Option<(i32, i32, i32, i32)> {
    let display = scrap::Display::all().ok()?.into_iter().nth(display_idx)?;
    let (x, y) = display.origin();
    Some((x, y, display.width() as i32, display.height() as i32))
}

fn screenshot_scrap(args: &Value) -> Result<Value, String> {
    use scrap::{Capturer, Display, TraitCapturer, TraitPixelBuffer};
    use hbb_common::base64::Engine;

    let display_idx = args["display"].as_u64().unwrap_or(0) as usize;

    let displays = Display::all().map_err(|e| format!("Failed to get displays: {}", e))?;
    if display_idx >= displays.len() {
        return Err(format!("Display {} not found (have {})", display_idx, displays.len()));
    }
    let display = displays.into_iter().nth(display_idx).unwrap();
    let w = display.width();
    let h = display.height();

    let mut capturer = Capturer::new(display).map_err(|e| format!("Failed to create capturer: {}", e))?;

    let mut frame_data = None;
    for _attempt in 0..30 {
        match capturer.frame(Duration::from_millis(200)) {
            Ok(frame) => {
                let pixbuf = match &frame {
                    scrap::Frame::PixelBuffer(pb) => pb,
                    _ => return Err("GPU texture frames not supported in MCP mode".to_string()),
                };

                let bgra = pixbuf.data();
                let stride = pixbuf.stride()[0];
                let mut rgba = Vec::with_capacity(w * h * 4);
                for y in 0..h {
                    for x in 0..w {
                        let i = stride * y + 4 * x;
                        if i + 3 < bgra.len() {
                            rgba.push(bgra[i + 2]);
                            rgba.push(bgra[i + 1]);
                            rgba.push(bgra[i]);
                            rgba.push(bgra[i + 3]);
                        }
                    }
                }
                frame_data = Some(rgba);
                break;
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(format!("Capture error: {}", e)),
        }
    }

    let rgba = frame_data.ok_or("Failed to capture frame after retries. The desktop may be locked or inaccessible.")?;

    let mut png = Vec::new();
    repng::encode(&mut png, w as _, h as _, &rgba)
        .map_err(|e| format!("PNG encode error: {}", e))?;

    let b64 = hbb_common::base64::engine::general_purpose::STANDARD.encode(&png);

    Ok(json!([{
        "type": "image",
        "data": b64,
        "mimeType": "image/png"
    }]))
}

fn capture_rgba(region: Option<(i32, i32, i32, i32)>) -> Result<(Vec<u8>, u32, u32), String> {

    #[cfg(windows)]
    let check_blank = region.is_none();
    #[cfg(windows)]
    {
        if let Ok(result) = capture_rgba_gdi(region) {
            if check_blank && is_blank_capture(&result.0) {
                return Err(blank_capture_error_message());
            }
            return Ok(result);
        }
    }
    let result = capture_rgba_scrap(region)?;
    #[cfg(windows)]
    if check_blank && is_blank_capture(&result.0) {
        return Err(blank_capture_error_message());
    }
    Ok(result)
}

#[cfg(windows)]
fn is_blank_capture(rgba: &[u8]) -> bool {
    if rgba.len() < 4 {
        return false;
    }

    let stride = (rgba.len() / (256 * 4)).max(1) * 4;
    let mut i = 0;
    while i + 3 < rgba.len() {
        if rgba[i] != 0 || rgba[i + 1] != 0 || rgba[i + 2] != 0 {
            return false;
        }
        i += stride;
    }
    true
}

#[cfg(windows)]
fn blank_capture_error_message() -> String {
    "Screen capture returned an empty (all-black) buffer. This usually means the \
     process has no access to the interactive desktop — e.g. it was launched from \
     an SSH session, a Windows service (Session 0), or before a user logged in. \
     Run HopToDesk from the user's interactive session (double-click, scheduled \
     task with the user's login token, or `psexec -i -s`).".to_string()
}

#[cfg(windows)]
fn capture_rgba_gdi(region: Option<(i32, i32, i32, i32)>) -> Result<(Vec<u8>, u32, u32), String> {
    #[allow(clashing_extern_declarations)]
    extern "system" {
        fn GetDC(hwnd: isize) -> isize;
        fn ReleaseDC(hwnd: isize, hdc: isize) -> i32;
        fn CreateCompatibleDC(hdc: isize) -> isize;
        fn CreateCompatibleBitmap(hdc: isize, w: i32, h: i32) -> isize;
        fn SelectObject(hdc: isize, obj: isize) -> isize;
        fn BitBlt(dst: isize, x: i32, y: i32, w: i32, h: i32, src: isize, sx: i32, sy: i32, rop: u32) -> i32;
        fn DeleteDC(hdc: isize) -> i32;
        fn DeleteObject(obj: isize) -> i32;
        fn GetSystemMetrics(idx: i32) -> i32;
        fn GetDIBits(hdc: isize, hbm: isize, start: u32, lines: u32, bits: *mut u8, bi: *mut u8, usage: u32) -> i32;
    }
    const SM_CXSCREEN: i32 = 0;
    const SM_CYSCREEN: i32 = 1;
    const SRCCOPY: u32 = 0x00CC0020;

    unsafe {
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        if screen_w <= 0 || screen_h <= 0 {
            return Err("No display available".to_string());
        }
        let (src_x, src_y, cap_w, cap_h) = match region {
            Some((rx, ry, rw, rh)) => {
                let cx = rx.max(0).min(screen_w);
                let cy = ry.max(0).min(screen_h);
                let cw = rw.min(screen_w - cx).max(1);
                let ch = rh.min(screen_h - cy).max(1);
                (cx, cy, cw, ch)
            }
            None => (0, 0, screen_w, screen_h),
        };
        let screen_dc = GetDC(0);
        if screen_dc == 0 { return Err("GetDC failed".to_string()); }
        let mem_dc = CreateCompatibleDC(screen_dc);
        let bitmap = CreateCompatibleBitmap(screen_dc, cap_w, cap_h);
        let old = SelectObject(mem_dc, bitmap);
        BitBlt(mem_dc, 0, 0, cap_w, cap_h, screen_dc, src_x, src_y, SRCCOPY);

        let mut bmi = [0u8; 44];
        bmi[0] = 40;
        bmi[4..8].copy_from_slice(&cap_w.to_le_bytes());
        let neg_h = (-cap_h).to_le_bytes();
        bmi[8..12].copy_from_slice(&neg_h);
        bmi[12..14].copy_from_slice(&1u16.to_le_bytes());
        bmi[14..16].copy_from_slice(&32u16.to_le_bytes());

        let mut pixels = vec![0u8; (cap_w * cap_h * 4) as usize];
        let ret = GetDIBits(mem_dc, bitmap, 0, cap_h as u32, pixels.as_mut_ptr(), bmi.as_mut_ptr(), 0);
        SelectObject(mem_dc, old);
        DeleteObject(bitmap);
        DeleteDC(mem_dc);
        ReleaseDC(0, screen_dc);
        if ret == 0 { return Err("GetDIBits failed".to_string()); }

        let mut rgba = Vec::with_capacity(pixels.len());
        for chunk in pixels.chunks(4) {
            rgba.push(chunk[2]);
            rgba.push(chunk[1]);
            rgba.push(chunk[0]);
            rgba.push(255);
        }
        Ok((rgba, cap_w as u32, cap_h as u32))
    }
}

fn capture_rgba_scrap(region: Option<(i32, i32, i32, i32)>) -> Result<(Vec<u8>, u32, u32), String> {
    use scrap::{Capturer, Display, TraitCapturer, TraitPixelBuffer};

    let displays = Display::all().map_err(|e| format!("Failed to get displays: {}", e))?;
    if displays.is_empty() {
        return Err("No displays found".to_string());
    }
    let display = displays.into_iter().next().unwrap();
    let w = display.width();
    let h = display.height();
    let mut capturer = Capturer::new(display).map_err(|e| format!("Failed to create capturer: {}", e))?;

    let mut frame_data = None;
    for _ in 0..30 {
        match capturer.frame(Duration::from_millis(200)) {
            Ok(frame) => {
                let pixbuf = match &frame {
                    scrap::Frame::PixelBuffer(pb) => pb,
                    _ => return Err("GPU texture frames not supported".to_string()),
                };
                let bgra = pixbuf.data();
                let stride = pixbuf.stride()[0];
                let mut rgba = Vec::with_capacity(w * h * 4);
                for row in 0..h {
                    for col in 0..w {
                        let i = stride * row + 4 * col;
                        if i + 3 < bgra.len() {
                            rgba.push(bgra[i + 2]);
                            rgba.push(bgra[i + 1]);
                            rgba.push(bgra[i]);
                            rgba.push(bgra[i + 3]);
                        }
                    }
                }
                frame_data = Some(rgba);
                break;
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(format!("Capture error: {}", e)),
        }
    }
    let full_rgba = frame_data.ok_or("Failed to capture frame after retries")?;

    if let Some((rx, ry, rw, rh)) = region {
        let rx = rx.max(0) as usize;
        let ry = ry.max(0) as usize;
        let rw = (rw as usize).min(w.saturating_sub(rx));
        let rh = (rh as usize).min(h.saturating_sub(ry));
        if rw == 0 || rh == 0 {
            return Err("Crop region out of bounds".to_string());
        }
        let mut cropped = Vec::with_capacity(rw * rh * 4);
        for row in ry..(ry + rh) {
            let start = (row * w + rx) * 4;
            let end = start + rw * 4;
            if end <= full_rgba.len() {
                cropped.extend_from_slice(&full_rgba[start..end]);
            }
        }
        return Ok((cropped, rw as u32, rh as u32));
    }

    Ok((full_rgba, w as u32, h as u32))
}

fn pixel_diff_percentage(buf1: &[u8], buf2: &[u8], threshold: u8) -> f64 {
    if buf1.len() != buf2.len() || buf1.is_empty() {
        return 100.0;
    }
    let pixel_count = buf1.len() / 4;
    let mut changed = 0usize;
    for i in (0..buf1.len()).step_by(4) {
        let dr = (buf1[i] as i16 - buf2[i] as i16).unsigned_abs() as u8;
        let dg = (buf1[i+1] as i16 - buf2[i+1] as i16).unsigned_abs() as u8;
        let db = (buf1[i+2] as i16 - buf2[i+2] as i16).unsigned_abs() as u8;
        if dr > threshold || dg > threshold || db > threshold {
            changed += 1;
        }
    }
    (changed as f64 / pixel_count as f64) * 100.0
}

fn grid_diff(buf1: &[u8], buf2: &[u8], width: u32, height: u32) -> Vec<Value> {
    let cols = 3u32;
    let rows = 3u32;
    let cell_w = width / cols;
    let cell_h = height / rows;
    let labels = ["top-left", "top-center", "top-right",
                  "middle-left", "center", "middle-right",
                  "bottom-left", "bottom-center", "bottom-right"];
    let mut regions = Vec::new();

    for gy in 0..rows {
        for gx in 0..cols {
            let mut changed_pixels = 0u32;
            let mut total_pixels = 0u32;
            let x0 = gx * cell_w;
            let y0 = gy * cell_h;
            let x1 = if gx == cols - 1 { width } else { x0 + cell_w };
            let y1 = if gy == rows - 1 { height } else { y0 + cell_h };
            for row in y0..y1 {
                for col in x0..x1 {
                    let i = ((row * width + col) * 4) as usize;
                    if i + 3 < buf1.len() && i + 3 < buf2.len() {
                        total_pixels += 1;
                        let dr = (buf1[i] as i16 - buf2[i] as i16).unsigned_abs();
                        let dg = (buf1[i+1] as i16 - buf2[i+1] as i16).unsigned_abs();
                        let db = (buf1[i+2] as i16 - buf2[i+2] as i16).unsigned_abs();
                        if dr > 10 || dg > 10 || db > 10 {
                            changed_pixels += 1;
                        }
                    }
                }
            }
            if total_pixels > 0 {
                let pct = (changed_pixels as f64 / total_pixels as f64) * 100.0;
                if pct > 0.5 {
                    let intensity = if pct > 30.0 { "high" } else if pct > 5.0 { "medium" } else { "low" };
                    let idx = (gy * cols + gx) as usize;
                    regions.push(json!({
                        "area": labels[idx],
                        "changed_percentage": (pct * 10.0).round() / 10.0,
                        "intensity": intensity,
                    }));
                }
            }
        }
    }
    regions
}

fn cleanup_diff_refs() {
    if let Ok(mut refs) = DIFF_REFS.lock() {
        refs.retain(|_, (_, _, _, created)| created.elapsed() < Duration::from_secs(60));
    }
}

fn tool_get_window_list() -> Result<Value, String> {
    let windows = get_window_list_platform()?;
    let text = serde_json::to_string_pretty(&windows).unwrap_or_default();
    Ok(json!([{ "type": "text", "text": text }]))
}

fn tool_mouse_click(args: &Value) -> Result<Value, String> {
    use enigo::{Enigo, MouseButton, MouseControllable};

    let x = args["x"].as_i64().ok_or("Missing x")? as i32;
    let y = args["y"].as_i64().ok_or("Missing y")? as i32;
    let button_str = args["button"].as_str().unwrap_or("left");
    let double = args["double_click"].as_bool().unwrap_or(false);

    let button = match button_str {
        "right" => MouseButton::Right,
        "middle" => MouseButton::Middle,
        _ => MouseButton::Left,
    };

    let mut enigo = Enigo::new();
    enigo.mouse_move_to(x, y);
    std::thread::sleep(Duration::from_millis(10));
    enigo.mouse_click(button);
    if double {
        std::thread::sleep(Duration::from_millis(50));
        enigo.mouse_click(button);
    }

    Ok(json!([{ "type": "text", "text": format!("Clicked ({}, {}) {}{}", x, y, button_str, if double { " (double)" } else { "" }) }]))
}

fn tool_mouse_move(args: &Value) -> Result<Value, String> {
    use enigo::{Enigo, MouseControllable};

    let x = args["x"].as_i64().ok_or("Missing x")? as i32;
    let y = args["y"].as_i64().ok_or("Missing y")? as i32;

    let mut enigo = Enigo::new();
    enigo.mouse_move_to(x, y);

    Ok(json!([{ "type": "text", "text": format!("Moved mouse to ({}, {})", x, y) }]))
}

fn tool_type_text(args: &Value) -> Result<Value, String> {
    use enigo::{Enigo, KeyboardControllable};

    let text = args["text"].as_str().ok_or("Missing text")?;
    let mut enigo = Enigo::new();
    enigo.key_sequence(text);

    Ok(json!([{ "type": "text", "text": format!("Typed {} characters", text.len()) }]))
}

fn tool_key_press(args: &Value) -> Result<Value, String> {
    use enigo::{Enigo, Key, KeyboardControllable};

    let key_str = args["key"].as_str().ok_or("Missing key")?;
    let modifiers = args["modifiers"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();

    let key = match key_str.to_lowercase().as_str() {
        "return" | "enter" => Key::Return,
        "escape" | "esc" => Key::Escape,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        "delete" => Key::Delete,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "space" => Key::Space,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        s if s.len() == 1 => Key::Layout(s.chars().next().unwrap()),
        _ => return Err(format!("Unknown key: {}", key_str)),
    };

    let mut enigo = Enigo::new();

    for m in &modifiers {
        let _ = match m.to_lowercase().as_str() {
            "ctrl" | "control" => enigo.key_down(Key::Control),
            "alt" => enigo.key_down(Key::Alt),
            "shift" => enigo.key_down(Key::Shift),
            "meta" | "super" | "win" | "cmd" => enigo.key_down(Key::Meta),
            _ => Ok(()),
        };
    }

    enigo.key_click(key);

    for m in modifiers.iter().rev() {
        match m.to_lowercase().as_str() {
            "ctrl" | "control" => enigo.key_up(Key::Control),
            "alt" => enigo.key_up(Key::Alt),
            "shift" => enigo.key_up(Key::Shift),
            "meta" | "super" | "win" | "cmd" => enigo.key_up(Key::Meta),
            _ => {}
        }
    }

    Ok(json!([{ "type": "text", "text": format!("Pressed {}{}", if modifiers.is_empty() { String::new() } else { format!("{}+", modifiers.join("+")) }, key_str) }]))
}

fn device_status_num() -> i32 {
    if crate::is_server() {
        let state = hbb_common::config::get_online_state();
        return if state > 0 { 1 } else { state as i32 };
    }
    crate::ui_interface::get_connect_status().status_num
}

fn tool_get_device_info() -> Result<Value, String> {
    let id = hbb_common::config::Config::get_id();
    let version = env!("CARGO_PKG_VERSION");
    let platform = std::env::consts::OS;
    let hostname = crate::common::hostname();
    let status_num = device_status_num();

    let status_text = match status_num {
        1 => "online",
        0 => "connecting",
        _ => "offline",
    };

    let info = json!({
        "device_id": id,
        "version": version,
        "platform": platform,
        "hostname": hostname,
        "status": status_text,
        "status_num": status_num
    });

    Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&info).unwrap_or_default() }]))
}

#[allow(unused_variables)]
fn tool_get_device_health() -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        match scrap::android::call_main_service_get_by_name("device_health") {
            Ok(s) => Ok(json!([{ "type": "text", "text": s }])),
            Err(e) => Err(format!("device_health failed: {:?}", e)),
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        Err("get_device_health is only supported on Android".to_string())
    }
}

#[allow(unused_variables)]
fn tool_android_global_action(arguments: &Value) -> Result<Value, String> {
    let action = arguments["action"].as_str().unwrap_or("home").to_string();
    #[cfg(target_os = "android")]
    {
        match scrap::android::call_main_service_set_by_name("global_action", Some(&action), None) {
            Ok(_) => Ok(json!([{ "type": "text", "text": format!("global_action {} dispatched", action) }])),
            Err(e) => Err(format!("global_action failed: {:?}", e)),
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        Err("android_global_action is only supported on Android".to_string())
    }
}

#[allow(unused_variables)]
fn tool_android_tap(arguments: &Value) -> Result<Value, String> {
    let x = arguments["x"].as_i64().unwrap_or(0) as i32;
    let y = arguments["y"].as_i64().unwrap_or(0) as i32;
    #[cfg(target_os = "android")]
    {
        let down = scrap::android::call_main_service_pointer_input("mouse", 9, x, y);
        let up = scrap::android::call_main_service_pointer_input("mouse", 10, x, y);
        match (down, up) {
            (Ok(_), Ok(_)) => Ok(json!([{ "type": "text", "text": format!("tapped {},{}", x, y) }])),
            _ => Err("android_tap failed".to_string()),
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        Err("android_tap is only supported on Android".to_string())
    }
}

fn tool_list_peers(args: &Value) -> Result<Value, String> {
    let filter = args["filter"].as_str().unwrap_or("all");
    let mut result = json!({});

    if filter == "recent" || filter == "all" {
        let peers: Vec<Value> = PeerConfig::peers(None)
            .into_iter()
            .map(|(id, modified, config)| {
                let last_seen = modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                json!({
                    "id": id,
                    "username": config.info.username,
                    "hostname": config.info.hostname,
                    "platform": config.info.platform,
                    "alias": config.options.get("alias").unwrap_or(&String::new()).clone(),
                    "last_seen_epoch": last_seen
                })
            })
            .collect();
        result["recent"] = json!(peers);
    }

    if filter == "favorites" || filter == "all" {
        let favs = crate::ui_interface::get_fav();
        result["favorites"] = json!(favs);
    }

    if filter == "lan" || filter == "all" {
        let lan = crate::ui_interface::get_lan_peers();
        let lan_json: Vec<Value> = lan.into_iter().map(|p| {
            json!({
                "id": p.get("id").cloned().unwrap_or_default(),
                "username": p.get("username").cloned().unwrap_or_default(),
                "hostname": p.get("hostname").cloned().unwrap_or_default(),
                "platform": p.get("platform").cloned().unwrap_or_default(),
            })
        }).collect();
        result["lan"] = json!(lan_json);
    }

    Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]))
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
fn tool_connect_to_peer(args: &Value) -> Result<Value, String> {
    let peer_id = args["peer_id"].as_str().ok_or("Missing peer_id")?;
    let conn_type = args["connection_type"].as_str().unwrap_or("connect");
    let password = args["password"].as_str().map(|s| s.to_string());

    match conn_type {
        "connect" | "file-transfer" | "port-forward" => {}
        _ => return Err(format!("Invalid connection_type: {}. Use: connect, file-transfer, port-forward", conn_type)),
    }

    crate::ui::new_remote(
        peer_id.to_string(),
        conn_type.to_string(),
        false,
        None,
        password,
    );

    Ok(json!([{ "type": "text", "text": format!("Connection initiated to {} (type: {})", peer_id, conn_type) }]))
}

#[cfg(any(feature = "flutter", feature = "cli"))]
fn tool_connect_to_peer(_args: &Value) -> Result<Value, String> {
    Err("connect_to_peer not available in this build".to_string())
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
fn tool_disconnect_peer(args: &Value) -> Result<Value, String> {
    let peer_id = args["peer_id"].as_str().ok_or("Missing peer_id")?;
    crate::ui::close_remote_connection(peer_id);
    Ok(json!([{ "type": "text", "text": format!("Disconnected peer {}", peer_id) }]))
}

#[cfg(any(feature = "flutter", feature = "cli"))]
fn tool_disconnect_peer(_args: &Value) -> Result<Value, String> {
    Err("disconnect_peer not available in this build".to_string())
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
fn tool_list_active_connections() -> Result<Value, String> {
    let connections: Vec<Value> = crate::ui::list_active_connections()
        .into_iter()
        .map(|(id, conn_type, alive)| {

            let connected = if alive {
                query_child_state(&id)
                    .map(|s| s["video"]["width"].as_i64().unwrap_or(0) > 0)
                    .unwrap_or(false)
            } else {
                false
            };
            json!({
                "peer_id": id,
                "connection_type": conn_type,
                "alive": alive,
                "connected": connected
            })
        })
        .collect();

    Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&connections).unwrap_or_default() }]))
}

#[cfg(any(feature = "flutter", feature = "cli"))]
fn tool_list_active_connections() -> Result<Value, String> {
    Err("list_active_connections not available in this build".to_string())
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
fn tool_get_ui_state(args: &Value) -> Result<Value, String> {
    let filter_peer = args["peer_id"].as_str();
    let connections = crate::ui::list_active_connections();
    let windows = get_window_list_platform()?;
    let windows_arr = windows.as_array().cloned().unwrap_or_default();

    let main_win = windows_arr.iter().find(|w| {
        w["title"].as_str() == Some("HopToDesk")
    }).cloned();

    let mut state_files: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    for (pid, _, alive) in &connections {
        if !alive { continue; }
        if let Some(filter) = filter_peer {
            if pid != filter { continue; }
        }
        if let Some(state) = query_child_state(pid) {
            state_files.insert(pid.clone(), state);
        }
    }

    let mut conn_states = Vec::new();
    for (peer_id, conn_type, alive) in &connections {
        if let Some(filter) = filter_peer {
            if peer_id != filter { continue; }
        }

        let win = windows_arr.iter().find(|w| {
            w["title"].as_str() == Some(peer_id.as_str())
        }).cloned();

        let toolbar = get_toolbar_buttons(conn_type);

        let connected = if let Some(rich_state) = state_files.get(peer_id) {

            rich_state["video"]["width"].as_i64().unwrap_or(0) > 0
        } else {
            false
        };

        let mut conn_state = json!({
            "peer_id": peer_id,
            "connection_type": conn_type,
            "alive": alive,
            "connected": connected,
            "window": win,
            "toolbar": toolbar,
        });

        if let Some(rich_state) = state_files.get(peer_id) {
            conn_state["state"] = rich_state.clone();
        }

        conn_states.push(conn_state);
    }

    let result = json!({
        "main_window": main_win,
        "connections": conn_states,
    });

    Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]))
}

#[cfg(any(feature = "flutter", feature = "cli"))]
fn tool_get_ui_state(_args: &Value) -> Result<Value, String> {
    Err("get_ui_state not available in this build".to_string())
}

fn tool_wait_for_event(args: &Value) -> Result<Value, String> {
    let event = args["event"].as_str().ok_or("Missing event")?;
    let peer_id = args["peer_id"].as_str().ok_or("Missing peer_id")?;
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(30000).min(60000);

    let start = std::time::Instant::now();
    let poll_interval = std::time::Duration::from_millis(500);

    loop {
        if start.elapsed().as_millis() as u64 >= timeout_ms {
            return Ok(json!([{ "type": "text", "text": json!({
                "event": event,
                "peer_id": peer_id,
                "status": "timeout",
                "elapsed_ms": start.elapsed().as_millis() as u64,
                "message": format!("Timed out waiting for '{}' after {}ms", event, timeout_ms)
            }).to_string() }]));
        }

        #[cfg(not(any(feature = "flutter", feature = "cli")))]
        {
            let connections = crate::ui::list_active_connections();
            let conn = connections.iter().find(|(pid, _, _)| pid == peer_id);

            match event {
                "connection_ready" => {
                    if let Some((_, _, alive)) = conn {
                        if *alive {

                            if let Some(state) = query_child_state(peer_id) {
                                let video_w = state["video"]["width"].as_i64().unwrap_or(0);
                                if video_w > 0 {
                                    return Ok(json!([{ "type": "text", "text": json!({
                                        "event": "connection_ready",
                                        "peer_id": peer_id,
                                        "status": "fired",
                                        "elapsed_ms": start.elapsed().as_millis() as u64,
                                        "state": state,
                                    }).to_string() }]));
                                }
                            }
                        }
                    }
                }
                "connection_closed" => {
                    if conn.is_none() || conn.map_or(false, |(_, _, alive)| !alive) {
                        return Ok(json!([{ "type": "text", "text": json!({
                            "event": "connection_closed",
                            "peer_id": peer_id,
                            "status": "fired",
                            "elapsed_ms": start.elapsed().as_millis() as u64,
                        }).to_string() }]));
                    }
                }
                "dialog_appeared" => {
                    if conn.is_some() {
                        if let Some(state) = query_child_state(peer_id) {
                            if state["ui"]["dialog_open"].as_bool() == Some(true) {
                                return Ok(json!([{ "type": "text", "text": json!({
                                    "event": "dialog_appeared",
                                    "peer_id": peer_id,
                                    "status": "fired",
                                    "elapsed_ms": start.elapsed().as_millis() as u64,
                                    "dialog_type": state["ui"]["dialog_type"],
                                    "state": state,
                                }).to_string() }]));
                            }
                        }
                    }
                }
                "dialog_dismissed" => {
                    if conn.is_some() {
                        if let Some(state) = query_child_state(peer_id) {
                            if state["ui"]["dialog_open"].as_bool() != Some(true) {
                                return Ok(json!([{ "type": "text", "text": json!({
                                    "event": "dialog_dismissed",
                                    "peer_id": peer_id,
                                    "status": "fired",
                                    "elapsed_ms": start.elapsed().as_millis() as u64,
                                    "state": state,
                                }).to_string() }]));
                            }
                        }
                    }
                }
                "quality_stable" => {
                    if conn.is_some() {
                        if let Some(state) = query_child_state(peer_id) {
                            let fps = state["connection"]["quality"]["fps"].as_i64().unwrap_or(0);
                            let delay = state["connection"]["quality"]["delay_ms"].as_i64().unwrap_or(9999);
                            if fps > 0 && delay < 500 {
                                return Ok(json!([{ "type": "text", "text": json!({
                                    "event": "quality_stable",
                                    "peer_id": peer_id,
                                    "status": "fired",
                                    "elapsed_ms": start.elapsed().as_millis() as u64,
                                    "fps": fps,
                                    "delay_ms": delay,
                                    "state": state,
                                }).to_string() }]));
                            }
                        }
                    }
                }
                _ => return Err(format!("Unknown event: {}", event)),
            }
        }

        #[cfg(any(feature = "flutter", feature = "cli"))]
        return Err("wait_for_event not available in this build".to_string());

        std::thread::sleep(poll_interval);
    }
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
fn query_child_state(peer_id: &str) -> Option<Value> {
    let path = std::env::temp_dir().join(format!("hoptodesk-mcp-state-{}.json", peer_id));
    let _ = std::fs::remove_file(&path);
    crate::ui::send_to_child(peer_id, "query_state");

    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < 1000 {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(parsed) = serde_json::from_str::<Value>(&content) {
                let _ = std::fs::remove_file(&path);
                return Some(parsed);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    None
}

fn tool_dismiss_dialog(args: &Value) -> Result<Value, String> {
    let peer_id = args["peer_id"].as_str().ok_or("Missing peer_id")?;

    #[cfg(not(any(feature = "flutter", feature = "cli")))]
    {
        if peer_id == "all" {
            let count = crate::ui::send_dismiss_to_all_children();
            return Ok(json!([{ "type": "text", "text": format!("Dismiss signal sent to {} connections", count) }]));
        }
        if crate::ui::send_to_child(peer_id, "dismiss") {
            return Ok(json!([{ "type": "text", "text": format!("Dismiss signal sent for peer {}", peer_id) }]));
        }
        return Err(format!("No active connection found for peer {}", peer_id));
    }
    #[cfg(any(feature = "flutter", feature = "cli"))]
    return Err("dismiss_dialog not available in this build".to_string());
}

fn tool_click_toolbar_button(args: &Value) -> Result<Value, String> {
    let peer_id = args["peer_id"].as_str().ok_or("Missing peer_id")?;
    let button_id = args["button_id"].as_str().ok_or("Missing button_id")?;

    let valid_buttons = ["fullscreen", "chat", "action", "display", "keyboard",
                         "recording", "securitycode", "transfer-file", "remote-print",
                         "screenshot", "switch-sides", "privacy-mode", "annotate"];
    let valid_action_items = ["request-elevation", "os-password", "tunnel", "ctrl-alt-del",
                              "restart_remote_device", "lock-screen", "block-input",
                              "take-screenshot", "refresh"];
    let valid_display_items = ["original", "shrink", "stretch", "adaptive", "best", "balanced",
                               "low", "custom", "view-only", "show-remote-cursor", "disable-audio",
                               "disable-clipboard", "lock-after-session-end", "privacy-mode",
                               "show-quality-monitor"];
    let valid_keyboard_items = ["auto", "map", "translate"];
    let valid_annotate_items = ["draw-pen", "draw-line", "draw-arrow", "draw-rect",
                                "draw-ellipse", "draw-undo", "draw-clear", "draw-exit"];

    let is_valid = if let Some(colon) = button_id.find(':') {
        let menu = &button_id[..colon];
        let item = &button_id[colon + 1..];
        match menu {
            "action" => valid_action_items.contains(&item),
            "display" => valid_display_items.contains(&item),
            "keyboard" => valid_keyboard_items.contains(&item),
            "annotate" => valid_annotate_items.contains(&item),
            _ => false,
        }
    } else {
        valid_buttons.contains(&button_id)
    };

    if !is_valid {
        return Err(format!(
            "Invalid button_id '{}'. Use a button ID (e.g. 'chat') or 'menu:item' for submenus (e.g. 'display:shrink', 'action:ctrl-alt-del', 'keyboard:map')",
            button_id
        ));
    }

    #[cfg(not(any(feature = "flutter", feature = "cli")))]
    {
        let cmd = format!("click_button:{}", button_id);
        if crate::ui::send_to_child(peer_id, &cmd) {
            return Ok(json!([{ "type": "text", "text": format!("Clicked '{}' on peer {}", button_id, peer_id) }]));
        }
        return Err(format!("No active connection found for peer {}", peer_id));
    }
    #[cfg(any(feature = "flutter", feature = "cli"))]
    return Err("click_toolbar_button not available in this build".to_string());
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
fn tool_transfer_file(args: &Value) -> Result<Value, String> {
    let peer_id = args["peer_id"].as_str().ok_or("Missing peer_id")?;
    let path = args["path"].as_str().ok_or("Missing path")?;
    let to = args["to"].as_str().ok_or("Missing to")?;
    let direction = args["direction"].as_str().unwrap_or("download");
    let is_remote = match direction {
        "download" => "1",
        "upload" => "0",
        _ => return Err("direction must be download or upload".to_string()),
    };
    if path.contains('|') || to.contains('|') {
        return Err("path and to cannot contain the | character".to_string());
    }
    let cmd = format!("transfer_file:{}|{}|{}", is_remote, path, to);
    if crate::ui::send_to_child(peer_id, &cmd) {
        return Ok(json!([{ "type": "text", "text": format!("{} of {} started on peer {}", direction, path, peer_id) }]));
    }
    Err(format!("No file transfer window open for peer {}. Use connect_to_peer with connection_type 'file-transfer' first.", peer_id))
}

#[cfg(any(feature = "flutter", feature = "cli"))]
fn tool_transfer_file(_args: &Value) -> Result<Value, String> {
    Err("transfer_file not available in this build".to_string())
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
fn tool_open_tunnel(args: &Value) -> Result<Value, String> {
    let peer_id = args["peer_id"].as_str().ok_or("Missing peer_id")?;
    let local_port = args["local_port"].as_i64().ok_or("Missing local_port")?;
    let remote_port = args["remote_port"].as_i64().ok_or("Missing remote_port")?;
    let remote_host = args["remote_host"].as_str().unwrap_or("localhost");
    if !(1..=65535).contains(&local_port) || !(1..=65535).contains(&remote_port) {
        return Err("ports must be between 1 and 65535".to_string());
    }
    if remote_host.contains('|') {
        return Err("remote_host cannot contain the | character".to_string());
    }
    let cmd = format!("open_tunnel:{}|{}|{}", local_port, remote_host, remote_port);
    if crate::ui::send_to_child(peer_id, &cmd) {
        return Ok(json!([{ "type": "text", "text": format!("Tunnel requested on peer {}: localhost:{} -> {}:{}", peer_id, local_port, remote_host, remote_port) }]));
    }
    Err(format!("No port forward window open for peer {}. Use connect_to_peer with connection_type 'port-forward' first.", peer_id))
}

#[cfg(any(feature = "flutter", feature = "cli"))]
fn tool_open_tunnel(_args: &Value) -> Result<Value, String> {
    Err("open_tunnel not available in this build".to_string())
}

#[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
fn tool_test_print(args: &Value) -> Result<Value, String> {
    #[cfg(target_os = "windows")]
    {
        use hbb_common::config::{keys, LocalConfig};

        let text = args["text"].as_str().unwrap_or("HopToDesk Remote Print Test\n\nThis is a test page to verify remote printing works.\nTimestamp: ");
        let printer = LocalConfig::get_option(keys::OPTION_PRINTER_SELECTED_NAME);
        if printer.is_empty() {
            return Err("No printer configured. Use set_remote_printer first.".to_string());
        }

        let result = print_text_to_printer(&printer, text);
        match result {
            Ok(path) => Ok(json!([{ "type": "text", "text": format!("Test print job sent to '{}'. Output: {}", printer, path) }])),
            Err(e) => Err(format!("Print failed: {}", e)),
        }
    }
    #[cfg(not(target_os = "windows"))]
    Err("test_print is only available on Windows".to_string())
}

#[cfg(target_os = "windows")]
fn print_text_to_printer(printer_name: &str, text: &str) -> Result<String, String> {
    use std::ptr;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    type HANDLE = *mut std::ffi::c_void;

    #[repr(C)]
    #[allow(non_snake_case)]
    struct DOCINFOW {
        cbSize: i32,
        lpszDocName: *const u16,
        lpszOutput: *const u16,
        lpszDatatype: *const u16,
        fwType: u32,
    }

    #[repr(C)]
    struct RECT { left: i32, top: i32, right: i32, bottom: i32 }

    #[allow(clashing_extern_declarations)]
    extern "system" {
        fn CreateDCW(driver: *const u16, device: *const u16, port: *const u16, devmode: *const u8) -> HANDLE;
        fn DeleteDC(hdc: HANDLE) -> i32;
        fn StartDocW(hdc: HANDLE, lpdi: *const DOCINFOW) -> i32;
        fn EndDoc(hdc: HANDLE) -> i32;
        fn StartPage(hdc: HANDLE) -> i32;
        fn EndPage(hdc: HANDLE) -> i32;
        fn DrawTextW(hdc: HANDLE, text: *const u16, count: i32, rect: *mut RECT, format: u32) -> i32;
        fn GetDeviceCaps(hdc: HANDLE, index: i32) -> i32;
    }

    fn to_wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    let output_path = if printer_name.to_lowercase().contains("print to pdf") {
        let docs = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\Public".into());
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Some(format!("{}\\Documents\\HopToDesk_Print_{}.pdf", docs, ts))
    } else {
        None
    };

    unsafe {
        let printer_wide = to_wide(printer_name);
        let hdc = CreateDCW(ptr::null(), printer_wide.as_ptr(), ptr::null(), ptr::null());
        if hdc.is_null() {
            return Err("Failed to create printer DC".to_string());
        }

        let doc_name = to_wide("HopToDesk Test Print");
        let output_wide;
        let output_ptr = if let Some(ref path) = output_path {
            output_wide = to_wide(path);
            output_wide.as_ptr()
        } else {
            ptr::null()
        };

        let di = DOCINFOW {
            cbSize: std::mem::size_of::<DOCINFOW>() as i32,
            lpszDocName: doc_name.as_ptr(),
            lpszOutput: output_ptr,
            lpszDatatype: ptr::null(),
            fwType: 0,
        };

        if StartDocW(hdc, &di) <= 0 {
            DeleteDC(hdc);
            return Err("StartDoc failed".to_string());
        }

        if StartPage(hdc) <= 0 {
            EndDoc(hdc);
            DeleteDC(hdc);
            return Err("StartPage failed".to_string());
        }

        let page_w = GetDeviceCaps(hdc, 8);
        let page_h = GetDeviceCaps(hdc, 10);

        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let full_text = format!("{}{}", text, ts);
        let text_wide = to_wide(&full_text);

        let mut rect = RECT {
            left: 100,
            top: 100,
            right: page_w - 100,
            bottom: page_h - 100,
        };

        DrawTextW(hdc, text_wide.as_ptr(), -1, &mut rect, 0x10);

        EndPage(hdc);
        EndDoc(hdc);
        DeleteDC(hdc);
    }

    Ok(output_path.unwrap_or_else(|| "sent to printer spool".to_string()))
}

#[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
fn tool_set_remote_printer(args: &Value) -> Result<Value, String> {
    #[cfg(target_os = "windows")]
    {
        use hbb_common::config::{keys, LocalConfig};

        let printers = crate::platform::get_printers();
        let printer_name = args["printer_name"].as_str().unwrap_or("");
        let auto_print = args["auto_print"].as_bool().unwrap_or(true);

        if printer_name.is_empty() {
            let current = LocalConfig::get_option(keys::OPTION_PRINTER_SELECTED_NAME);
            let auto_enabled = LocalConfig::get_option(keys::OPTION_PRINTER_ALLOW_AUTO_PRINT) == "Y";
            let result = json!({
                "printers": printers,
                "current_printer": current,
                "auto_print_enabled": auto_enabled,
            });
            return Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]));
        }

        let selected = if printer_name == "auto" {
            if printers.is_empty() {
                return Err("No printers available".to_string());
            }
            if printers.len() > 1 {
                return Err(format!("Multiple printers found ({}), specify one: {:?}", printers.len(), printers));
            }
            printers[0].clone()
        } else {

            if !printers.iter().any(|p| p == printer_name) {
                return Err(format!("Printer '{}' not found. Available: {:?}", printer_name, printers));
            }
            printer_name.to_string()
        };

        LocalConfig::set_option(keys::OPTION_PRINTER_SELECTED_NAME.to_string(), selected.clone());
        if auto_print {
            LocalConfig::set_option(keys::OPTION_PRINTER_ALLOW_AUTO_PRINT.to_string(), "Y".to_string());
        } else {
            LocalConfig::set_option(keys::OPTION_PRINTER_ALLOW_AUTO_PRINT.to_string(), "".to_string());
        }

        let msg = format!("Printer set to '{}', auto-print: {}. Remote print jobs will now be sent directly to this printer.", selected, if auto_print { "enabled" } else { "disabled" });
        Ok(json!([{ "type": "text", "text": msg }]))
    }
    #[cfg(not(target_os = "windows"))]
    Err("set_remote_printer is only available on Windows".to_string())
}

fn get_toolbar_buttons(conn_type: &str) -> Value {
    let is_file_transfer = conn_type == "file-transfer";
    let is_view_camera = conn_type == "view-camera";

    let mut buttons = Vec::new();

    if !is_file_transfer {
        buttons.push("fullscreen");
    }
    if !is_view_camera && !is_file_transfer {
        buttons.push("chat");
        buttons.push("action");
    }
    if !is_file_transfer {
        buttons.push("display");
    }
    if !is_view_camera && !is_file_transfer {
        buttons.push("keyboard");
        buttons.push("transfer-file");
        buttons.push("remote-print");
        buttons.push("screenshot");
    }

    let items: Vec<Value> = buttons.iter().map(|id| {
        let mut item = json!({
            "id": id,
            "label": get_button_label(id),
        });
        add_submenu_info(&mut item, id);
        item
    }).collect();

    json!({
        "buttons": items,
        "how_to_click": "Use click_toolbar_button with button_id for top-level buttons, or 'menu:item' for submenus (e.g. 'display:shrink', 'action:ctrl-alt-del', 'keyboard:map').",
        "keyboard_shortcut_print": "Ctrl+P opens Remote Print dialog",
    })
}

fn get_button_label(id: &str) -> &str {
    match id {
        "fullscreen" => "Full Screen",
        "screens" => "Display Selector (security + remote ID + monitor buttons)",
        "chat" => "Chat",
        "action" => "Control Actions (lightning bolt)",
        "display" => "Display Settings (monitor icon)",
        "keyboard" => "Keyboard mode",
        "recording" => "Recording (red dot toggle)",
        "securitycode" => "Security Code (shield)",
        "transfer-file" => "Transfer File",
        "remote-print" => "Remote Print (printer icon)",
        "screenshot" => "Screenshot (camera icon, copies to clipboard)",
        "switch-sides" => "Switch Sides",
        "privacy-mode" => "Privacy Mode (eye icon toggle)",
        _ => id,
    }
}

fn add_submenu_info(item: &mut Value, id: &str) {
    match id {
        "action" => {
            item["submenu"] = json!([
                {"id": "request-elevation", "label": "Request Elevation (Windows remote only)"},
                {"id": "os-password", "label": "OS Password"},
                {"id": "tunnel", "label": "TCP Tunneling"},
                {"id": "ctrl-alt-del", "label": "Insert Ctrl+Alt+Del"},
                {"id": "restart_remote_device", "label": "Restart Remote Device"},
                {"id": "lock-screen", "label": "Lock Screen"},
                {"id": "block-input", "label": "Block user input (Windows and Mac remote)"},
                {"id": "take-screenshot", "label": "Take screenshot (saves to file, shows dialog)"},
                {"id": "refresh", "label": "Refresh video"}
            ]);
            item["tip"] = json!("Use click_toolbar_button with button_id 'menu:item' format, e.g. 'action:ctrl-alt-del'");
        }
        "display" => {
            item["submenu"] = json!([
                {"id": "original", "label": "Original", "type": "view-style"},
                {"id": "shrink", "label": "Shrink", "type": "view-style"},
                {"id": "stretch", "label": "Stretch", "type": "view-style"},
                {"id": "best", "label": "Good image quality", "type": "image-quality"},
                {"id": "balanced", "label": "Balanced", "type": "image-quality"},
                {"id": "low", "label": "Optimize reaction time", "type": "image-quality"},
                {"id": "custom", "label": "Custom quality", "type": "image-quality"},
                {"id": "view-only", "label": "View only", "type": "toggle"},
                {"id": "show-remote-cursor", "label": "Show remote cursor", "type": "toggle"},
                {"id": "disable-audio", "label": "Mute", "type": "toggle"},
                {"id": "disable-clipboard", "label": "Disable clipboard", "type": "toggle"},
                {"id": "lock-after-session-end", "label": "Lock after session end", "type": "toggle"}
            ]);
            item["tip"] = json!("Use click_toolbar_button with button_id 'menu:item' format, e.g. 'display:shrink'");
        }
        "keyboard" => {
            item["submenu"] = json!([
                {"id": "auto", "label": "Auto (default)"},
                {"id": "map", "label": "Remote layout"},
                {"id": "translate", "label": "My layout"}
            ]);
            item["tip"] = json!("Use click_toolbar_button with button_id 'menu:item' format, e.g. 'keyboard:auto'");
        }
        "annotate" => {
            item["submenu"] = json!([
                {"id": "draw-pen", "label": "Pen (freehand)"},
                {"id": "draw-line", "label": "Line"},
                {"id": "draw-arrow", "label": "Arrow"},
                {"id": "draw-rect", "label": "Rectangle"},
                {"id": "draw-ellipse", "label": "Ellipse"},
                {"id": "draw-undo", "label": "Undo last drawing"},
                {"id": "draw-clear", "label": "Clear drawings"},
                {"id": "draw-exit", "label": "Exit draw mode"}
            ]);
            item["tip"] = json!("Use click_toolbar_button with button_id 'menu:item' format, e.g. 'annotate:draw-rect'; bare 'annotate' toggles draw mode");
        }
        _ => {}
    }
}

#[cfg(windows)]
fn get_window_list_platform() -> Result<Value, String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    extern "system" {
        fn EnumWindows(cb: extern "system" fn(isize, isize) -> i32, lparam: isize) -> i32;
        fn GetWindowTextW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
        fn GetWindowRect(hwnd: isize, rect: *mut [i32; 4]) -> i32;
        fn IsWindowVisible(hwnd: isize) -> i32;
    }

    static mut WINDOWS: Vec<Value> = Vec::new();

    extern "system" fn enum_cb(hwnd: isize, _: isize) -> i32 {
        unsafe {
            if IsWindowVisible(hwnd) == 0 {
                return 1;
            }
            let mut buf = [0u16; 512];
            let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), 512);
            if len <= 0 {
                return 1;
            }
            let title = OsString::from_wide(&buf[..len as usize])
                .to_string_lossy()
                .to_string();
            if title.is_empty() {
                return 1;
            }
            let mut rect = [0i32; 4];
            GetWindowRect(hwnd, &mut rect);
            (*std::ptr::addr_of_mut!(WINDOWS)).push(json!({
                "title": title,
                "x": rect[0],
                "y": rect[1],
                "width": rect[2] - rect[0],
                "height": rect[3] - rect[1]
            }));
        }
        1
    }

    unsafe {
        WINDOWS = Vec::new();
        EnumWindows(enum_cb, 0);
        Ok(json!((*std::ptr::addr_of!(WINDOWS)).clone()))
    }
}

#[cfg(target_os = "macos")]
fn get_window_list_platform() -> Result<Value, String> {
    let windows: Vec<Value> = crate::platform::macos::on_screen_windows()
        .into_iter()
        .filter(|w| w["layer"].as_i64() == Some(0))
        .map(|w| {
            json!({
                "title": w["title"],
                "app": w["app"],
                "x": w["x"],
                "y": w["y"],
                "width": w["width"],
                "height": w["height"]
            })
        })
        .collect();
    Ok(json!(windows))
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn get_window_list_platform() -> Result<Value, String> {
    use std::process::Command;
    let output = Command::new("wmctrl")
        .arg("-lG")
        .output()
        .map_err(|e| format!("wmctrl error (install with: apt install wmctrl): {}", e))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let windows: Vec<Value> = text.lines().filter_map(parse_wmctrl_line).collect();
    Ok(json!(windows))
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn parse_wmctrl_line(line: &str) -> Option<Value> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 7 {
        return None;
    }
    Some(json!({
        "title": fields[7..].join(" "),
        "x": fields[2].parse::<i32>().ok()?,
        "y": fields[3].parse::<i32>().ok()?,
        "width": fields[4].parse::<i32>().ok()?,
        "height": fields[5].parse::<i32>().ok()?
    }))
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux", target_os = "freebsd")))]
fn get_window_list_platform() -> Result<Value, String> {
    Err("Window listing not supported on this platform".to_string())
}

fn tool_exec_operation(args: &Value) -> Result<Value, String> {
    let operation = args["operation"].as_str().unwrap_or("");
    if operation.is_empty() {
        return Err("operation is required".to_string());
    }

    let dashboard_user_id = hbb_common::config::Config::get_option("dashboard_user_id");
    if dashboard_user_id.is_empty() {
        return Err("Remote execution requires dashboard enrollment".to_string());
    }

    eprintln!("[exec] Running operation: {}", operation);

    let result = match operation {
        "get_system_info" => exec_get_system_info(),
        "disk_usage" => exec_disk_usage(),
        "list_processes" => exec_list_processes(args),
        "network_info" => exec_network_info(),
        "get_service_logs" => exec_get_service_logs(args),
        "ping_test" => exec_ping_test(args),
        "installed_software" => exec_installed_software(),
        "get_security_posture" => exec_get_security_posture(),
        "restart_hoptodesk" => exec_restart_hoptodesk(),
        "kill_process" => exec_kill_process(args),
        "flush_dns" => exec_flush_dns(),
        "reboot_device" => exec_reboot_device(),
        "shutdown_device" => exec_shutdown_device(),
        "clear_temp_files" => exec_clear_temp_files(),
        "run_update_check" => exec_run_update_check(),
        "list_services" => exec_list_services(),
        "get_event_logs" => exec_get_event_logs(args),
        "restart_service" => exec_restart_service(args),
        "update_client" => exec_update_client(args),
        _ => Err(format!("Unknown operation: {}", operation)),
    };

    match result {
        Ok(output) => {
            let response = json!({
                "operation": operation,
                "exit_code": 0,
                "output": output
            });
            Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&response).unwrap_or_default() }]))
        }
        Err(msg) => {
            let response = json!({
                "operation": operation,
                "exit_code": 1,
                "output": msg
            });
            Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&response).unwrap_or_default() }]))
        }
    }
}

fn cut_at_char_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn read_to_end_in_background<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            pipe.read_to_end(&mut bytes).ok();
        }
        tx.send(bytes).ok();
    });
    rx
}

fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("taskkill")
            .args(&["/T", "/F", "/PID", &child.id().to_string()])
            .creation_flags(winapi::um::winbase::CREATE_NO_WINDOW)
            .output()
            .ok();
    }
    #[cfg(unix)]
    unsafe {
        hbb_common::libc::kill(-(child.id() as hbb_common::libc::pid_t), hbb_common::libc::SIGKILL);
    }
    child.kill().ok();
    child.wait().ok();
}

fn run_with_timeout(
    mut command: std::process::Command,
    timeout_secs: u64,
) -> Result<std::process::Output, String> {
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(winapi::um::winbase::CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to spawn command: {}", e))?;
    let stdout = read_to_end_in_background(child.stdout.take());
    let stderr = read_to_end_in_background(child.stderr.take());
    let timed_out = || format!("Command timed out after {}s", timeout_secs);
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                kill_process_tree(&mut child);
                return Err(timed_out());
            }
            Err(e) => return Err(format!("Command failed: {}", e)),
        }
    };
    let output_deadline = deadline.max(Instant::now() + Duration::from_secs(1));
    let collect = |pipe: std::sync::mpsc::Receiver<Vec<u8>>| {
        pipe.recv_timeout(output_deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| timed_out())
    };
    Ok(std::process::Output {
        status,
        stdout: collect(stdout)?,
        stderr: collect(stderr)?,
    })
}

fn run_command(cmd: &str, args: &[&str], timeout_secs: u64) -> Result<String, String> {
    let mut command = std::process::Command::new(cmd);
    command.args(args).stdin(std::process::Stdio::null());
    let output = run_with_timeout(command, timeout_secs)
        .map_err(|e| format!("Failed to execute {}: {}", cmd, e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        Ok(if stdout.is_empty() { stderr } else { stdout })
    } else {
        if !stderr.is_empty() {
            Err(format!("Exit code {}: {}", output.status.code().unwrap_or(-1), stderr.trim()))
        } else {
            Err(format!("Exit code {}", output.status.code().unwrap_or(-1)))
        }
    }
}

fn exec_get_system_info() -> Result<String, String> {
    #[cfg(windows)]
    {
        let mut info = String::new();
        if let Ok(o) = run_command("cmd", &["/C", "systeminfo"], 60) {
            info.push_str(&o);
        }
        Ok(info)
    }
    #[cfg(target_os = "macos")]
    {
        let mut info = String::new();
        if let Ok(o) = run_command("sw_vers", &[], 5) { info.push_str(&o); info.push('\n'); }
        if let Ok(o) = run_command("sysctl", &["-n", "hw.memsize"], 5) { info.push_str(&format!("Memory: {} bytes\n", o.trim())); }
        if let Ok(o) = run_command("uptime", &[], 5) { info.push_str(&o); }
        Ok(info)
    }
    #[cfg(target_os = "linux")]
    {
        let mut info = String::new();
        if let Ok(o) = run_command("uname", &["-a"], 5) { info.push_str(&o); }
        if let Ok(o) = run_command("free", &["-h"], 5) { info.push_str(&o); }
        if let Ok(o) = run_command("uptime", &[], 5) { info.push_str(&o); }
        Ok(info)
    }
    #[cfg(target_os = "freebsd")]
    {
        let mut info = String::new();
        if let Ok(o) = run_command("uname", &["-a"], 5) { info.push_str(&o); }
        if let Ok(o) = run_command("sysctl", &["-n", "hw.physmem"], 5) { info.push_str(&format!("Memory: {} bytes\n", o.trim())); }
        if let Ok(o) = run_command("uptime", &[], 5) { info.push_str(&o); }
        Ok(info)
    }
}

fn exec_disk_usage() -> Result<String, String> {
    #[cfg(windows)]
    {
        const DISK_USAGE: &str = "$ProgressPreference='SilentlyContinue';\
            Get-CimInstance Win32_LogicalDisk -Filter 'DriveType=3'|Format-Table DeviceID,\
            @{n='SizeGB';e={[math]::Round($_.Size/1GB,1)}},@{n='FreeGB';e={[math]::Round($_.FreeSpace/1GB,1)}},\
            @{n='UsedPct';e={if($_.Size){[math]::Round(100-$_.FreeSpace*100/$_.Size)}}} -AutoSize|Out-String -Width 160";
        run_command(
            "powershell",
            &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", DISK_USAGE],
            20,
        )
    }
    #[cfg(not(windows))]
    { run_command("df", &["-h"], 10) }
}

#[allow(unused_variables)]
fn exec_list_processes(args: &Value) -> Result<String, String> {
    let limit = args["limit"].as_u64().unwrap_or(20) as usize;
    let sort_by = args["sort_by"].as_str().unwrap_or("cpu");

    #[cfg(windows)]
    {
        let key = if sort_by == "memory" { "WorkingSet64" } else { "CPU" };
        let cmd = format!(
            "$ProgressPreference='SilentlyContinue';Get-Process|Sort-Object {} -Descending|Select-Object -First {} \
             Name,Id,@{{n='CPU_s';e={{[math]::Round($_.CPU,1)}}}},@{{n='MemMB';e={{[math]::Round($_.WorkingSet64/1MB)}}}}|Format-Table -AutoSize|Out-String -Width 160",
            key, limit.min(100)
        );
        run_command(
            "powershell",
            &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &cmd],
            20,
        )
    }
    #[cfg(target_os = "macos")]
    {
        let sort_flag = if sort_by == "memory" { "-m" } else { "-r" };
        let cmd = format!("ps aux {} | head -n {} | cut -c1-240", sort_flag, limit + 1);
        run_command("sh", &["-c", &cmd], 10)
    }
    #[cfg(target_os = "linux")]
    {
        let sort_key = if sort_by == "memory" { "--sort=-%mem" } else { "--sort=-%cpu" };
        let cmd = format!("ps aux {} | head -n {} | cut -c1-240", sort_key, limit + 1);
        run_command("sh", &["-c", &cmd], 10)
    }
    #[cfg(target_os = "freebsd")]
    {
        let sort_flag = if sort_by == "memory" { "-m" } else { "-r" };
        let cmd = format!("ps aux {} | head -n {} | cut -c1-240", sort_flag, limit + 1);
        run_command("sh", &["-c", &cmd], 10)
    }
}

fn exec_network_info() -> Result<String, String> {
    #[cfg(windows)]
    { run_command("cmd", &["/C", "ipconfig /all"], 15) }
    #[cfg(target_os = "macos")]
    { run_command("sh", &["-c", "ifconfig | grep -E 'flags|inet'"], 10) }
    #[cfg(target_os = "linux")]
    { run_command("sh", &["-c", "ip addr show 2>/dev/null || ifconfig"], 10) }
    #[cfg(target_os = "freebsd")]
    { run_command("ifconfig", &[], 10) }
}

fn exec_get_service_logs(args: &Value) -> Result<String, String> {
    let lines = args["lines"].as_u64().unwrap_or(50).min(200) as usize;
    let mut newest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    let mut dirs = vec![hbb_common::config::Config::log_path()];
    #[cfg(windows)]
    {
        let app_name = crate::get_app_name();
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
        let users_root = std::path::PathBuf::from(format!("{}\\Users", drive));
        if let Ok(users) = std::fs::read_dir(&users_root) {
            for user in users.flatten() {
                let log_dir = user
                    .path()
                    .join("AppData")
                    .join("Roaming")
                    .join(&app_name)
                    .join("log");
                if log_dir.is_dir() {
                    dirs.push(log_dir);
                }
            }
        }
    }
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().map_or(false, |ext| ext == "log") {
                if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                    if newest.as_ref().map_or(true, |(t, _)| modified > *t) {
                        newest = Some((modified, path));
                    }
                }
            }
        }
    }
    let Some((_, path)) = newest else {
        return Ok("No log file found".to_string());
    };
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    let all: Vec<&str> = content.lines().collect();
    let start = all.len().saturating_sub(lines);
    Ok(format!("{}\n\n{}", path.display(), all[start..].join("\n")))
}

fn exec_ping_test(args: &Value) -> Result<String, String> {
    let host = args["host"].as_str().unwrap_or("").trim();
    if host.is_empty() {
        return Err("host parameter is required".to_string());
    }

    if host.chars().any(|c| ";|&`$(){}[]\\!#~<>\"'".contains(c)) {
        return Err("Invalid host".to_string());
    }
    let count = args["count"].as_u64().unwrap_or(4).min(10);

    #[cfg(windows)]
    { run_command("ping", &["-n", &count.to_string(), host], 60) }
    #[cfg(not(windows))]
    { run_command("ping", &["-c", &count.to_string(), host], 60) }
}

fn exec_installed_software() -> Result<String, String> {
    #[cfg(windows)]
    {
        use winreg::enums::*;
        use winreg::RegKey;
        let roots = [
            (HKEY_LOCAL_MACHINE, "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall", KEY_READ | KEY_WOW64_64KEY),
            (HKEY_LOCAL_MACHINE, "SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall", KEY_READ),
            (HKEY_CURRENT_USER, "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall", KEY_READ),
        ];
        let mut apps: Vec<Value> = Vec::new();
        for (hive, path, flags) in roots.iter() {
            let root = RegKey::predef(*hive);
            if let Ok(uninstall) = root.open_subkey_with_flags(path, *flags) {
                for name in uninstall.enum_keys().flatten() {
                    if let Ok(sub) = uninstall.open_subkey_with_flags(&name, *flags) {
                        let display: Result<String, _> = sub.get_value("DisplayName");
                        if let Ok(dn) = display {
                            if dn.trim().is_empty() {
                                continue;
                            }
                            let system_component: Result<u32, _> = sub.get_value("SystemComponent");
                            if let Ok(1) = system_component {
                                continue;
                            }
                            let version: String = sub.get_value("DisplayVersion").unwrap_or_default();
                            apps.push(json!({ "DisplayName": dn, "DisplayVersion": version }));
                        }
                    }
                }
            }
        }
        let mut serialized = serde_json::to_string(&apps).unwrap_or_else(|_| "[]".to_string());
        while serialized.len() > 50000 && apps.len() > 1 {
            let keep = (apps.len() * 50000 / serialized.len()).max(1);
            apps.truncate(keep);
            serialized = serde_json::to_string(&apps).unwrap_or_else(|_| "[]".to_string());
        }
        Ok(serialized)
    }
    #[cfg(target_os = "macos")]
    { run_command("sh", &["-c", "ls /Applications/ && echo '---' && brew list --versions 2>/dev/null || true"], 15) }
    #[cfg(target_os = "linux")]
    { run_command("sh", &["-c", "dpkg -l 2>/dev/null | head -100 || rpm -qa 2>/dev/null | head -100 || echo 'Package manager not found'"], 15) }
    #[cfg(target_os = "freebsd")]
    { run_command("sh", &["-c", "pkg info 2>/dev/null | head -100 || echo 'Package manager not found'"], 15) }
}

fn exec_get_security_posture() -> Result<String, String> {
    #[cfg(windows)]
    {
        use winreg::enums::*;
        use winreg::RegKey;
        let firewall = match run_command("netsh", &["advfirewall", "show", "allprofiles", "state"], 15) {
            Ok(o) => {
                let mut any_on = false;
                let mut any_off = false;
                for line in o.lines() {
                    let l = line.to_ascii_uppercase();
                    if l.contains("STATE") {
                        if l.contains("OFF") {
                            any_off = true;
                        } else if l.contains("ON") {
                            any_on = true;
                        }
                    }
                }
                if any_off { "off" } else if any_on { "on" } else { "unknown" }
            }
            Err(_) => "unknown",
        };
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        let sysnative_bde = format!("{}\\Sysnative\\manage-bde.exe", system_root);
        let manage_bde = if std::path::Path::new(&sysnative_bde).exists() { sysnative_bde.as_str() } else { "manage-bde" };
        let disk_encryption = match run_command(manage_bde, &["-status", "C:"], 30) {
            Ok(o) => {
                let u = o.to_ascii_uppercase();
                if u.contains("PROTECTION ON") { "on" } else if u.contains("PROTECTION OFF") { "off" } else { "unknown" }
            }
            Err(_) => "unknown",
        };
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        let native_view = KEY_READ | KEY_WOW64_64KEY;
        let pending_reboot = hklm
            .open_subkey_with_flags("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\WindowsUpdate\\Auto Update\\RebootRequired", native_view)
            .is_ok()
            || hklm
                .open_subkey_with_flags("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Component Based Servicing\\RebootPending", native_view)
                .is_ok();
        Ok(json!({ "firewall": firewall, "disk_encryption": disk_encryption, "pending_reboot": pending_reboot }).to_string())
    }
    #[cfg(target_os = "macos")]
    {
        let firewall = match run_command("/usr/libexec/ApplicationFirewall/socketfilterfw", &["--getglobalstate"], 10) {
            Ok(o) => {
                let l = o.to_ascii_lowercase();
                if l.contains("state = 1") || (l.contains("enabled") && !l.contains("disabled")) { "on" } else { "off" }
            }
            Err(_) => "unknown",
        };
        let disk_encryption = match run_command("fdesetup", &["status"], 10) {
            Ok(o) => {
                let u = o.to_ascii_uppercase();
                if u.contains("FILEVAULT IS ON") { "on" } else if u.contains("FILEVAULT IS OFF") { "off" } else { "unknown" }
            }
            Err(_) => "unknown",
        };
        Ok(json!({ "firewall": firewall, "disk_encryption": disk_encryption, "pending_reboot": null }).to_string())
    }
    #[cfg(target_os = "linux")]
    {
        let firewall = {
            let ufw = run_command("sh", &["-c", "ufw status 2>/dev/null"], 10).unwrap_or_default();
            if ufw.to_ascii_lowercase().contains("status: active") {
                "on"
            } else {
                let fw = run_command("sh", &["-c", "systemctl is-active firewalld 2>/dev/null"], 10).unwrap_or_default();
                if fw.trim() == "active" { "on" } else { "off" }
            }
        };
        let disk_encryption = {
            let types = run_command("sh", &["-c", "lsblk -rno TYPE 2>/dev/null"], 10).unwrap_or_default();
            if types.lines().any(|x| x.trim() == "crypt") { "on" } else { "off" }
        };
        let pending_reboot = std::path::Path::new("/var/run/reboot-required").exists();
        Ok(json!({ "firewall": firewall, "disk_encryption": disk_encryption, "pending_reboot": pending_reboot }).to_string())
    }
    #[cfg(target_os = "freebsd")]
    {
        let firewall = {
            let pf = run_command("sh", &["-c", "pfctl -s info 2>/dev/null"], 10).unwrap_or_default();
            if pf.contains("Status: Enabled") { "on" } else { "off" }
        };
        let disk_encryption = {
            let geli = run_command("sh", &["-c", "geli status 2>/dev/null"], 10).unwrap_or_default();
            if geli.trim().is_empty() { "off" } else { "on" }
        };
        let pending_reboot = false;
        Ok(json!({ "firewall": firewall, "disk_encryption": disk_encryption, "pending_reboot": pending_reboot }).to_string())
    }
}

fn exec_restart_hoptodesk() -> Result<String, String> {
    eprintln!("[exec] Restarting {} service", crate::get_app_name());
    #[cfg(windows)]
    {
        let service_name = crate::get_app_name();
        run_command(
            "cmd",
            &[
                "/C",
                &format!(
                    "net stop \"{}\" & net start \"{}\"",
                    service_name, service_name
                ),
            ],
            120,
        )
    }
    #[cfg(target_os = "macos")]
    {
        let label = format!("{}_service", crate::get_full_name().replace(' ', ""));
        run_command(
            "sh",
            &[
                "-c",
                &format!(
                    "launchctl kickstart -kp system/{} 2>/dev/null || echo 'Service restart requested'",
                    label
                ),
            ],
            15,
        )
    }
    #[cfg(target_os = "linux")]
    {
        let service_name = crate::get_app_name_lower();
        run_command(
            "sh",
            &[
                "-c",
                &format!(
                    "systemctl restart {} 2>/dev/null || echo 'Service restart requested'",
                    service_name
                ),
            ],
            15,
        )
    }
    #[cfg(target_os = "freebsd")]
    {
        let service_name = crate::get_app_name().to_lowercase();
        run_command(
            "sh",
            &[
                "-c",
                &format!(
                    "service {} restart 2>/dev/null || echo 'Service restart requested'",
                    service_name
                ),
            ],
            15,
        )
    }
}

fn exec_kill_process(args: &Value) -> Result<String, String> {
    let process_name = args["process_name"].as_str().unwrap_or("");
    let pid = args["pid"].as_u64();

    if process_name.is_empty() && pid.is_none() {
        return Err("process_name or pid is required".to_string());
    }

    if !process_name.is_empty() && process_name.chars().any(|c| ";|&`$(){}[]\\!#~<>\"'".contains(c)) {
        return Err("Invalid process name".to_string());
    }

    if let Some(pid) = pid {
        #[cfg(windows)]
        { run_command("taskkill", &["/PID", &pid.to_string(), "/F"], 10) }
        #[cfg(not(windows))]
        { run_command("kill", &["-9", &pid.to_string()], 10) }
    } else {
        #[cfg(windows)]
        { run_command("taskkill", &["/IM", &format!("{}.exe", process_name), "/F"], 10) }
        #[cfg(not(windows))]
        { run_command("sh", &["-c", &format!("pkill -f '{}'", process_name)], 10) }
    }
}

fn exec_flush_dns() -> Result<String, String> {
    #[cfg(windows)]
    { run_command("cmd", &["/C", "ipconfig /flushdns"], 10) }
    #[cfg(target_os = "macos")]
    { run_command("sh", &["-c", "sudo dscacheutil -flushcache && sudo killall -HUP mDNSResponder && echo 'DNS cache flushed'"], 10) }
    #[cfg(target_os = "linux")]
    { run_command("sh", &["-c", "systemd-resolve --flush-caches 2>/dev/null && echo 'DNS cache flushed' || echo 'DNS flush not supported'"], 10) }
    #[cfg(target_os = "freebsd")]
    { run_command("sh", &["-c", "service local_unbound restart 2>/dev/null && echo 'DNS cache flushed' || echo 'DNS flush not supported'"], 10) }
}

fn exec_reboot_device() -> Result<String, String> {
    eprintln!("[exec] REBOOT requested");
    #[cfg(windows)]
    { run_command("shutdown", &["/r", "/t", "5", "/c", "Reboot requested from HopToDesk Dashboard"], 5) }
    #[cfg(target_os = "linux")]
    {
        std::thread::spawn(reboot_through_platform)
            .join()
            .map_err(|_| "Restart failed".to_string())?
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    { power_action(system_shutdown::reboot, "Restart") }
}

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn reboot_through_platform() -> Result<String, String> {
    crate::platform::reboot()
        .await
        .map(|_| "Restart started".to_string())
        .map_err(|e| format!("Restart failed: {}", e))
}

#[cfg(not(windows))]
fn power_action(action: fn() -> system_shutdown::ShutdownResult, verb: &str) -> Result<String, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(action());
    });
    match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(())) => Ok(format!("{verb} started")),
        Ok(Err(e)) => Err(format!("{verb} failed: {e}")),
        Err(_) => Ok(format!("{verb} requested, the system has not confirmed it yet")),
    }
}

fn exec_shutdown_device() -> Result<String, String> {
    eprintln!("[exec] SHUTDOWN requested");
    #[cfg(windows)]
    { run_command("shutdown", &["/s", "/t", "60", "/c", "Shutdown requested from HopToDesk Dashboard"], 5) }
    #[cfg(not(windows))]
    { power_action(system_shutdown::shutdown, "Shut down") }
}

fn exec_clear_temp_files() -> Result<String, String> {
    #[cfg(windows)]
    {
        let temp = std::env::var("TEMP").unwrap_or_else(|_| "C:\\Windows\\Temp".to_string());
        run_command("cmd", &["/C", &format!("del /q /s \"{}\\*\" 2>nul & echo Temp files cleared", temp)], 60)
    }
    #[cfg(target_os = "macos")]
    { run_command("sh", &["-c", "rm -rf /tmp/com.apple.* ~/Library/Caches/com.apple.* 2>/dev/null; du -sh /tmp/ ~/Library/Caches/ 2>/dev/null || echo 'Temp files cleared'"], 30) }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    { run_command("sh", &["-c", "rm -rf /tmp/hoptodesk-* 2>/dev/null; du -sh /tmp/ 2>/dev/null || echo 'Temp files cleared'"], 30) }
}

fn exec_update_client(args: &Value) -> Result<String, String> {
    #[cfg(not(windows))]
    {
        let _ = args;
        Err("Client update is supported on Windows only".to_string())
    }
    #[cfg(windows)]
    {
        let url_path = args["url"].as_str().unwrap_or("").to_string();
        let sha256 = args["sha256"].as_str().unwrap_or("").to_ascii_lowercase();
        let app_name = args["app_name"].as_str().unwrap_or("").to_string();
        let build_id = args["build_id"].as_u64().unwrap_or(0);
        if url_path.is_empty() || sha256.len() != 64 || app_name.is_empty() || build_id == 0 {
            return Err("url, sha256, app_name and build_id are required".to_string());
        }
        let current_build = hbb_common::config::custom_build_id();
        if current_build.is_empty() {
            return Err("This device runs a standard client, not a custom build".to_string());
        }
        if app_name != crate::get_app_name() {
            return Err(format!("Build is for {}, this device runs {}", app_name, crate::get_app_name()));
        }
        if current_build == build_id.to_string() {
            return Ok(format!("Already on build {}", build_id));
        }
        if !crate::platform::is_installed() {
            return Err("The client is not installed on this device, so it cannot be updated in place".to_string());
        }
        if !url_path.starts_with("/downloads/custom-builds/") {
            return Err("Download link must be a custom build on this dashboard".to_string());
        }
        let full_url = format!("{}{}", crate::dashboard::dashboard_origin(), url_path);
        let bytes = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
            rt.block_on(async {
                let response = crate::common::make_http_client()
                    .get(&full_url)
                    .send()
                    .await
                    .map_err(|e| format!("Download failed: {}", e))?;
                if !response.status().is_success() {
                    return Err(format!("Download failed: HTTP {}", response.status()));
                }
                response.bytes().await.map_err(|e| format!("Download failed: {}", e))
            })
        })
        .join()
        .map_err(|_| "Download thread failed".to_string())??;
        use sha2::{Digest, Sha256};
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if digest != sha256 {
            return Err("Downloaded file does not match the expected checksum".to_string());
        }
        let updater = std::env::temp_dir().join(format!(
            "{}-update-{}.exe",
            crate::get_app_name().replace(' ', ""),
            build_id
        ));
        std::fs::write(&updater, &bytes).map_err(|e| format!("Could not save the update: {}", e))?;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            use std::os::windows::process::CommandExt;
            let _ = std::process::Command::new(&updater)
                .arg("--silent-install")
                .creation_flags(winapi::um::winbase::CREATE_NO_WINDOW | winapi::um::winbase::DETACHED_PROCESS)
                .spawn();
        });
        Ok(format!(
            "Update to build {} started; the device reconnects when the install finishes",
            build_id
        ))
    }
}

fn exec_list_services() -> Result<String, String> {
    #[cfg(windows)]
    {
        const LIST_SERVICES: &str = "$ProgressPreference='SilentlyContinue';[Console]::OutputEncoding=[Text.Encoding]::UTF8;$t=[char]9;\
            Get-CimInstance Win32_Service|Sort-Object DisplayName|ForEach-Object{\
            $_.Name+$t+$_.DisplayName+$t+$_.State+$t+$_.StartMode}";
        run_command(
            "powershell",
            &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", LIST_SERVICES],
            30,
        )
    }
    #[cfg(target_os = "linux")]
    {
        let units = run_command(
            "systemctl",
            &["list-units", "--type=service", "--all", "--no-pager", "--no-legend", "--plain"],
            15,
        )?;
        Ok(units.lines().filter_map(systemd_unit_row).collect::<Vec<_>>().join("\n"))
    }
    #[cfg(target_os = "macos")]
    {
        let agents = run_command("launchctl", &["list"], 15)?;
        Ok(agents.lines().skip(1).filter_map(launchd_row).collect::<Vec<_>>().join("\n"))
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    { Err("Not supported on this system".to_string()) }
}

#[cfg(target_os = "macos")]
fn launchd_row(line: &str) -> Option<String> {
    let mut fields = line.split('\t');
    let pid = fields.next()?;
    let _status = fields.next()?;
    let label = fields.next()?.trim();
    if label.is_empty() {
        return None;
    }
    let state = if pid.trim().parse::<u32>().is_ok() { "running" } else { "stopped" };
    Some(format!("{}\t{}\t{}\t", label, label, state))
}

#[cfg(target_os = "macos")]
fn launchd_state(label: &str) -> String {
    run_command("launchctl", &["list"], 15)
        .ok()
        .and_then(|agents| {
            agents
                .lines()
                .skip(1)
                .filter_map(launchd_row)
                .find(|row| row.starts_with(&format!("{}\t", label)))
        })
        .and_then(|row| row.split('\t').nth(2).map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(target_os = "linux")]
fn systemd_unit_row(line: &str) -> Option<String> {
    let mut fields = line.split_whitespace();
    let unit = fields.next()?;
    let _load = fields.next()?;
    let _active = fields.next()?;
    let state = fields.next()?;
    let description = fields.collect::<Vec<_>>().join(" ");
    Some(format!("{}\t{}\t{}\t", unit, description, state))
}

#[allow(unused_variables)]
fn exec_get_event_logs(args: &Value) -> Result<String, String> {
    let log = if args["log"].as_str() == Some("Application") { "Application" } else { "System" };
    let level = args["level"].as_str().unwrap_or("error");
    let count = args["count"].as_u64().unwrap_or(50).clamp(1, 200).to_string();

    #[cfg(windows)]
    {
        let level_filter = match level {
            "error" => ";Level=1,2",
            "warning" => ";Level=1,2,3",
            _ => "",
        };
        let script = [
            "$ProgressPreference='SilentlyContinue';[Console]::OutputEncoding=[Text.Encoding]::UTF8;$t=[char]9;\
             Get-WinEvent -FilterHashtable @{LogName='",
            log,
            "'",
            level_filter,
            "} -MaxEvents ",
            &count,
            " -ErrorAction SilentlyContinue|ForEach-Object{\
             $m=[string]$_.Message -replace '\\s+',' ';\
             if($m.Length -gt 300){$m=$m.Substring(0,300)};\
             $_.TimeCreated.ToUniversalTime().ToString('yyyy-MM-dd\\THH:mm:ss\\Z')\
             +$t+$_.Level+$t+$_.Id+$t+$_.ProviderName+$t+$m};exit 0",
        ]
        .concat();
        run_command(
            "powershell",
            &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script],
            30,
        )
    }
    #[cfg(target_os = "linux")]
    {
        let mut journal_args = vec!["-n", count.as_str(), "-r", "--no-pager", "-o", "json"];
        if log == "Application" {
            journal_args.push("--user");
        }
        match level {
            "error" => journal_args.extend(["-p", "3"]),
            "warning" => journal_args.extend(["-p", "4"]),
            _ => {}
        }
        let entries = run_command("journalctl", &journal_args, 30)?;
        Ok(entries.lines().filter_map(journal_entry_row).collect::<Vec<_>>().join("\n"))
    }
    #[cfg(target_os = "macos")]
    {
        let only_errors = "(messageType == error OR messageType == fault)";
        let not_apple = "process != \"kernel\" AND NOT (subsystem BEGINSWITH \"com.apple.\")";
        let predicate = match (log, level) {
            ("Application", "all") => format!("eventType == logEvent AND {}", not_apple),
            ("Application", _) => format!("eventType == logEvent AND {} AND {}", only_errors, not_apple),
            (_, "all") => "eventType == logEvent".to_string(),
            _ => format!("eventType == logEvent AND {}", only_errors),
        };
        let window = if level == "all" { "5m" } else { "15m" };
        let rows_plus_summary_line = count.parse::<u64>().unwrap_or(50) + 1;
        let script = format!(
            "/usr/bin/log show --last {} --style ndjson --predicate '{}' 2>/dev/null | tail -n {}",
            window, predicate, rows_plus_summary_line
        );
        let entries = run_command("sh", &["-c", &script], 30)?;
        Ok(entries.lines().rev().filter_map(unified_log_row).collect::<Vec<_>>().join("\n"))
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    { Err("Not supported on this system".to_string()) }
}

#[cfg(target_os = "macos")]
fn unified_log_row(line: &str) -> Option<String> {
    let entry: Value = serde_json::from_str(line).ok()?;
    let time = chrono::DateTime::parse_from_str(entry["timestamp"].as_str()?, "%Y-%m-%d %H:%M:%S%.f%z")
        .ok()?
        .with_timezone(&chrono::Utc)
        .format("%Y-%m-%dT%H:%M:%SZ");
    let level = match entry["messageType"].as_str() {
        Some("Fault") => 1,
        Some("Error") => 2,
        _ => 4,
    };
    let source = entry["processImagePath"]
        .as_str()
        .and_then(|path| path.rsplit('/').next())
        .unwrap_or("");
    let pid = entry["processID"].as_u64().map(|pid| pid.to_string()).unwrap_or_default();
    let message: String = entry["eventMessage"]
        .as_str()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect();
    Some(format!("{}\t{}\t{}\t{}\t{}", time, level, pid, source, message))
}

fn exec_restart_service(args: &Value) -> Result<String, String> {
    let name = args["name"].as_str().unwrap_or("").trim();
    if !is_service_name(name) {
        return Err("Invalid service name".to_string());
    }
    #[cfg(windows)]
    {
        let script = format!(
            "$ProgressPreference='SilentlyContinue';$WarningPreference='SilentlyContinue';Restart-Service -Name '{}' -Force -ErrorAction Stop;(Get-Service -Name '{}').Status",
            name, name
        );
        run_command(
            "powershell",
            &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script],
            60,
        )
    }
    #[cfg(target_os = "linux")]
    {
        let unit = name.to_string();
        std::thread::spawn(move || restart_service_through_root_service(unit))
            .join()
            .map_err(|_| "Service request failed".to_string())?
    }
    #[cfg(target_os = "macos")]
    {
        let uid = run_command("id", &["-u"], 5)?.trim().to_string();
        let target = format!("gui/{}/{}", uid, name);
        run_command("launchctl", &["kickstart", "-k", &target], 60)
            .map_err(|e| format!("{} (a system service needs an administrator on this Mac)", e))?;
        Ok(launchd_state(name))
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    { Err("Not supported on this system".to_string()) }
}

pub fn is_service_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.chars().all(|c| c.is_ascii_alphanumeric() || " _.@()-".contains(c))
}

#[cfg(target_os = "linux")]
pub fn restart_service_as_root(name: &str) -> Result<String, String> {
    if !is_service_name(name) {
        return Err("Invalid service name".to_string());
    }
    run_command("systemctl", &["restart", name], 60)?;
    run_command("systemctl", &["is-active", name], 10).map(|state| state.trim().to_string())
}

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn restart_service_through_root_service(name: String) -> Result<String, String> {
    let mut stream = crate::ipc::connect(1000, crate::POSTFIX_SERVICE)
        .await
        .map_err(|e| format!("Service not reachable: {}", e))?;
    stream
        .send(&crate::ipc::Data::RestartService { name, output: None, error: None })
        .await
        .map_err(|e| format!("Service request failed: {}", e))?;
    match stream.next_timeout(70_000).await {
        Ok(Some(crate::ipc::Data::RestartService { output: Some(output), .. })) => Ok(output),
        Ok(Some(crate::ipc::Data::RestartService { error: Some(error), .. })) => Err(error),
        _ => Err("No answer from the service".to_string()),
    }
}

#[cfg(target_os = "linux")]
fn journal_entry_row(line: &str) -> Option<String> {
    let entry: Value = serde_json::from_str(line).ok()?;
    let micros: i64 = entry["__REALTIME_TIMESTAMP"].as_str()?.parse().ok()?;
    let time = chrono::DateTime::<chrono::Utc>::from_timestamp(micros / 1_000_000, 0)?
        .format("%Y-%m-%dT%H:%M:%SZ");
    let level = match entry["PRIORITY"].as_str().and_then(|p| p.parse::<u8>().ok()) {
        Some(0..=2) => 1,
        Some(3) => 2,
        Some(4) => 3,
        _ => 4,
    };
    let source = entry["SYSLOG_IDENTIFIER"]
        .as_str()
        .or_else(|| entry["_COMM"].as_str())
        .unwrap_or("");
    let message: String = entry["MESSAGE"]
        .as_str()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect();
    Some(format!("{}\t{}\t\t{}\t{}", time, level, source, message))
}

fn exec_run_update_check() -> Result<String, String> {
    #[cfg(windows)]
    {
        const WUA_SEARCH: &str = "$ErrorActionPreference='Stop';\
            $r=(New-Object -ComObject Microsoft.Update.Session).CreateUpdateSearcher().Search('IsInstalled=0 and IsHidden=0');\
            $t=@($r.Updates|ForEach-Object{$_.Title}|Select-Object -First 50);\
            [pscustomobject]@{count=$r.Updates.Count;titles=$t}|ConvertTo-Json -Compress";
        run_command(
            "powershell",
            &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", WUA_SEARCH],
            150,
        )
    }
    #[cfg(target_os = "macos")]
    { run_command("sh", &["-c", "softwareupdate -l 2>&1 | head -20"], 60) }
    #[cfg(target_os = "linux")]
    { run_command("sh", &["-c", "apt list --upgradable 2>/dev/null | head -20 || yum check-update 2>/dev/null | head -20 || echo 'Package manager not found'"], 60) }
    #[cfg(target_os = "freebsd")]
    { run_command("sh", &["-c", "pkg upgrade -n 2>/dev/null | head -20 || echo 'Package manager not found'"], 60) }
}

fn tool_run_command(args: &Value) -> Result<Value, String> {
    let command = args["command"].as_str().unwrap_or("");
    if command.is_empty() {
        return Err("command is required".to_string());
    }

    let dashboard_user_id = hbb_common::config::Config::get_option("dashboard_user_id");
    if dashboard_user_id.is_empty() {
        return Err("run_command requires dashboard enrollment".to_string());
    }

    let timeout_secs = args["timeout"].as_u64().unwrap_or(30).min(120);

    eprintln!("[mcp] run_command: {:?} (timeout: {}s)", command, timeout_secs);

    let (program, flag) = if cfg!(windows) { ("cmd", "/C") } else { ("sh", "-c") };
    let mut shell = std::process::Command::new(program);
    shell.args(&[flag, command]);

    match run_with_timeout(shell, timeout_secs) {
        Ok(output) => {
            let max_bytes = 64 * 1024;
            let stdout_raw = String::from_utf8_lossy(&output.stdout);
            let stderr_raw = String::from_utf8_lossy(&output.stderr);
            let truncated = stdout_raw.len() > max_bytes || stderr_raw.len() > max_bytes;
            let stdout = cut_at_char_boundary(&stdout_raw, max_bytes);
            let stderr = cut_at_char_boundary(&stderr_raw, max_bytes);
            let exit_code = output.status.code().unwrap_or(-1);

            let response = json!({
                "command": command,
                "exit_code": exit_code,
                "stdout": stdout,
                "stderr": stderr,
                "truncated": truncated
            });
            Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&response).unwrap_or_default() }]))
        }
        Err(msg) => {
            let response = json!({
                "command": command,
                "exit_code": -1,
                "stdout": "",
                "stderr": msg,
                "truncated": false
            });
            Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&response).unwrap_or_default() }]))
        }
    }
}

fn tool_get_clipboard() -> Result<Value, String> {
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|e| format!("Failed to access clipboard: {}", e))?;
    let text = clipboard.get_text()
        .unwrap_or_default();
    Ok(json!([{ "type": "text", "text": text }]))
}

fn tool_set_clipboard(args: &Value) -> Result<Value, String> {
    let text = args["text"].as_str().ok_or("Missing text")?;
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|e| format!("Failed to access clipboard: {}", e))?;
    clipboard.set_text(text)
        .map_err(|e| format!("Failed to set clipboard: {}", e))?;
    Ok(json!([{ "type": "text", "text": format!("Clipboard set ({} chars)", text.len()) }]))
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
fn tool_send_chat_message(args: &Value) -> Result<Value, String> {
    let peer_id = args["peer_id"].as_str().ok_or("Missing peer_id")?;
    let text = args["text"].as_str().ok_or("Missing text")?;
    let cmd = format!("chat:{}", text);
    if crate::ui::send_to_child(peer_id, &cmd) {
        Ok(json!([{ "type": "text", "text": format!("Chat message sent to peer {}", peer_id) }]))
    } else {
        Err(format!("No active connection found for peer {}", peer_id))
    }
}

#[cfg(any(feature = "flutter", feature = "cli"))]
fn tool_send_chat_message(_args: &Value) -> Result<Value, String> {
    Err("send_chat_message not available in this build".to_string())
}

fn tool_switch_permission(args: &Value) -> Result<Value, String> {
    let conn_id = args["conn_id"].as_i64().ok_or("Missing conn_id")? as i32;
    let permission = args["permission"].as_str().ok_or("Missing permission")?;
    let enabled = args["enabled"].as_bool().ok_or("Missing enabled")?;

    let valid = ["keyboard", "clipboard", "audio", "file", "restart", "recording"];
    if !valid.contains(&permission) {
        return Err(format!("Invalid permission '{}'. Valid: {:?}", permission, valid));
    }

    crate::ui_cm_interface::switch_permission(conn_id, permission.to_string(), enabled);
    Ok(json!([{ "type": "text", "text": format!("Permission '{}' {} for connection {}", permission, if enabled { "enabled" } else { "disabled" }, conn_id) }]))
}

fn tool_list_incoming_connections() -> Result<Value, String> {
    let clients = crate::ui_cm_interface::CLIENTS.read().map_err(|e| format!("Lock failed: {}", e))?;
    let connections: Vec<Value> = clients.iter().map(|(id, client)| {
        json!({
            "conn_id": id,
            "peer_id": client.peer_id,
            "name": client.name,
            "authorized": client.authorized,
            "disconnected": client.disconnected,
            "keyboard": client.keyboard,
            "clipboard": client.clipboard,
            "audio": client.audio,
            "file": client.file,
            "restart": client.restart,
            "recording": client.recording,
        })
    }).collect();
    drop(clients);
    Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&connections).unwrap_or_default() }]))
}

fn tool_list_local_files(args: &Value) -> Result<Value, String> {
    let path = args["path"].as_str().unwrap_or("");
    let include_hidden = args["include_hidden"].as_bool().unwrap_or(false);

    let dir = if path.is_empty() {
        #[cfg(windows)]
        { std::path::PathBuf::from(std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string())) }
        #[cfg(not(windows))]
        { std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string())) }
    } else {
        std::path::PathBuf::from(path)
    };

    if !dir.exists() {
        return Err(format!("Path does not exist: {}", dir.display()));
    }
    if !dir.is_dir() {
        return Err(format!("Not a directory: {}", dir.display()));
    }

    let mut entries = Vec::new();
    let read_dir = std::fs::read_dir(&dir)
        .map_err(|e| format!("Failed to read directory: {}", e))?;

    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = meta.as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        entries.push(json!({
            "name": name,
            "type": if is_dir { "directory" } else { "file" },
            "size": size,
            "modified_epoch": modified,
        }));
    }

    entries.sort_by(|a, b| {
        let a_dir = a["type"].as_str() == Some("directory");
        let b_dir = b["type"].as_str() == Some("directory");
        b_dir.cmp(&a_dir).then_with(|| {
            a["name"].as_str().unwrap_or("").to_lowercase()
                .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
        })
    });

    let result = json!({
        "path": dir.display().to_string(),
        "count": entries.len(),
        "entries": entries,
    });
    Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]))
}

fn tool_read_local_file(args: &Value) -> Result<Value, String> {
    let path = args["path"].as_str().ok_or("Missing path")?;
    let file_path = std::path::Path::new(path);

    if !file_path.exists() {
        return Err(format!("File does not exist: {}", path));
    }
    if !file_path.is_file() {
        return Err(format!("Not a file: {}", path));
    }

    let meta = std::fs::metadata(file_path)
        .map_err(|e| format!("Failed to read metadata: {}", e))?;
    if meta.len() > 1_048_576 {
        return Err(format!("File too large ({} bytes, max 1MB)", meta.len()));
    }

    let bytes = std::fs::read(file_path)
        .map_err(|e| format!("Failed to read file: {}", e))?;

    match String::from_utf8(bytes.clone()) {
        Ok(text) => Ok(json!([{ "type": "text", "text": text }])),
        Err(_) => {
            use hbb_common::base64::Engine;
            let b64 = hbb_common::base64::engine::general_purpose::STANDARD.encode(&bytes);
            Ok(json!([{ "type": "text", "text": format!("base64:{}", b64) }]))
        }
    }
}

#[derive(Clone)]
struct FileTransferStatus {
    status: String,
    bytes: u64,
    error: Option<String>,
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
fn set_transfer_status(id: &str, status: &str, bytes: u64, error: Option<String>) {
    if let Ok(mut map) = FILE_TRANSFERS.lock() {
        if map.len() > 100 {
            let done: Vec<String> = map
                .iter()
                .filter(|(_, v)| v.status != "pending")
                .map(|(k, _)| k.clone())
                .collect();
            for k in done {
                map.remove(&k);
            }
        }
        map.insert(
            id.to_string(),
            FileTransferStatus { status: status.to_string(), bytes, error },
        );
    }
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
fn transfer_http_client() -> Result<reqwest::blocking::Client, String> {
    let mut builder = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(1800));
    if let Some(conf) = hbb_common::config::Config::get_socks() {
        let proxy_url = hbb_common::proxy::proxy_url(&conf);
        if let Ok(proxy) = reqwest::Proxy::all(&proxy_url) {
            builder = builder.proxy(proxy);
        }
    }
    builder.build().map_err(|e| format!("Failed to build HTTP client: {}", e))
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
fn tool_upload_file_to_url(args: &Value) -> Result<Value, String> {
    let path = args["path"].as_str().ok_or("Missing path")?.to_string();
    let url = args["url"].as_str().ok_or("Missing url")?.to_string();
    let transfer_id = args["transfer_id"].as_str().ok_or("Missing transfer_id")?.to_string();
    let max_bytes = args["max_bytes"].as_u64().unwrap_or(0);
    if !url.starts_with("https://") {
        return Err("url must be an https URL".to_string());
    }
    let file_path = std::path::Path::new(&path);
    if !file_path.is_file() {
        return Err(format!("Not a file: {}", path));
    }
    let size = std::fs::metadata(file_path)
        .map_err(|e| format!("Failed to read metadata: {}", e))?
        .len();
    if max_bytes > 0 && size > max_bytes {
        return Err(format!("File too large ({} bytes, max {})", size, max_bytes));
    }

    set_transfer_status(&transfer_id, "pending", 0, None);
    let tid = transfer_id.clone();
    std::thread::spawn(move || {
        let result = (|| -> Result<u64, String> {
            let client = transfer_http_client()?;
            let file = std::fs::File::open(&path).map_err(|e| format!("Failed to open file: {}", e))?;
            let resp = client
                .put(&url)
                .header("Content-Type", "application/octet-stream")
                .body(file)
                .send()
                .map_err(|e| format!("Upload request failed: {}", e))?;
            if !resp.status().is_success() {
                return Err(format!("Upload rejected: HTTP {}", resp.status().as_u16()));
            }
            Ok(size)
        })();
        match result {
            Ok(n) => set_transfer_status(&tid, "complete", n, None),
            Err(e) => set_transfer_status(&tid, "failed", 0, Some(e)),
        }
    });

    Ok(json!([{ "type": "text", "text": format!("Upload started for transfer {}", transfer_id) }]))
}

#[cfg(any(feature = "flutter", feature = "cli"))]
fn tool_upload_file_to_url(_args: &Value) -> Result<Value, String> {
    Err("upload_file_to_url not available in this build".to_string())
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
fn tool_download_file_from_url(args: &Value) -> Result<Value, String> {
    let url = args["url"].as_str().ok_or("Missing url")?.to_string();
    let to = args["to"].as_str().ok_or("Missing to")?.to_string();
    let transfer_id = args["transfer_id"].as_str().ok_or("Missing transfer_id")?.to_string();
    let overwrite = args["overwrite"].as_bool().unwrap_or(false);
    let max_bytes = args["max_bytes"].as_u64().unwrap_or(0);
    if !url.starts_with("https://") {
        return Err("url must be an https URL".to_string());
    }
    let dest = std::path::Path::new(&to);
    if dest.exists() && !overwrite {
        return Err(format!("Destination already exists: {}", to));
    }

    set_transfer_status(&transfer_id, "pending", 0, None);
    let tid = transfer_id.clone();
    std::thread::spawn(move || {
        let result = (|| -> Result<u64, String> {
            let client = transfer_http_client()?;
            let mut resp = client.get(&url).send().map_err(|e| format!("Download request failed: {}", e))?;
            if !resp.status().is_success() {
                return Err(format!("Download failed: HTTP {}", resp.status().as_u16()));
            }
            if max_bytes > 0 {
                if let Some(len) = resp.content_length() {
                    if len > max_bytes {
                        return Err(format!("File too large ({} bytes, max {})", len, max_bytes));
                    }
                }
            }
            let tmp = format!("{}.hopdl-{}", to, tid);
            let mut file = std::fs::File::create(&tmp).map_err(|e| format!("Failed to create file: {}", e))?;
            let mut buf = [0u8; 65536];
            let mut total: u64 = 0;
            loop {
                let n = std::io::Read::read(&mut resp, &mut buf).map_err(|e| format!("Download read failed: {}", e))?;
                if n == 0 {
                    break;
                }
                total += n as u64;
                if max_bytes > 0 && total > max_bytes {
                    drop(file);
                    let _ = std::fs::remove_file(&tmp);
                    return Err(format!("File exceeded max {} bytes", max_bytes));
                }
                std::io::Write::write_all(&mut file, &buf[..n]).map_err(|e| format!("Write failed: {}", e))?;
            }
            drop(file);
            std::fs::rename(&tmp, &to).map_err(|e| format!("Failed to finalize file: {}", e))?;
            Ok(total)
        })();
        match result {
            Ok(n) => set_transfer_status(&tid, "complete", n, None),
            Err(e) => set_transfer_status(&tid, "failed", 0, Some(e)),
        }
    });

    Ok(json!([{ "type": "text", "text": format!("Download started for transfer {}", transfer_id) }]))
}

#[cfg(any(feature = "flutter", feature = "cli"))]
fn tool_download_file_from_url(_args: &Value) -> Result<Value, String> {
    Err("download_file_from_url not available in this build".to_string())
}

fn tool_get_file_transfer_status(args: &Value) -> Result<Value, String> {
    let transfer_id = args["transfer_id"].as_str().ok_or("Missing transfer_id")?;
    let status = FILE_TRANSFERS
        .lock()
        .ok()
        .and_then(|m| m.get(transfer_id).cloned());
    let obj = match status {
        Some(s) => json!({ "status": s.status, "bytes": s.bytes, "error": s.error }),
        None => json!({ "status": "unknown", "bytes": 0 }),
    };
    Ok(json!([{ "type": "text", "text": obj.to_string() }]))
}

fn tool_scroll(args: &Value) -> Result<Value, String> {
    use enigo::{Enigo, MouseControllable};

    let x = args["x"].as_i64().unwrap_or(-1) as i32;
    let y = args["y"].as_i64().unwrap_or(-1) as i32;
    let direction = args["direction"].as_str().unwrap_or("down");
    let amount = args["amount"].as_i64().unwrap_or(3) as i32;

    let mut enigo = Enigo::new();
    if x >= 0 && y >= 0 {
        enigo.mouse_move_to(x, y);
        std::thread::sleep(Duration::from_millis(10));
    }

    match direction {
        "up" => MouseControllable::mouse_scroll_y(&mut enigo, -amount),
        "down" => MouseControllable::mouse_scroll_y(&mut enigo, amount),
        "left" => MouseControllable::mouse_scroll_x(&mut enigo, -amount),
        "right" => MouseControllable::mouse_scroll_x(&mut enigo, amount),
        _ => return Err(format!("Invalid direction: {}. Use up/down/left/right.", direction)),
    }

    Ok(json!([{ "type": "text", "text": format!("Scrolled {} by {} lines at ({}, {})", direction, amount, x, y) }]))
}

fn tool_mouse_drag(args: &Value) -> Result<Value, String> {
    use enigo::{Enigo, MouseButton, MouseControllable};

    let start_x = args["start_x"].as_i64().ok_or("Missing start_x")? as i32;
    let start_y = args["start_y"].as_i64().ok_or("Missing start_y")? as i32;
    let end_x = args["end_x"].as_i64().ok_or("Missing end_x")? as i32;
    let end_y = args["end_y"].as_i64().ok_or("Missing end_y")? as i32;
    let steps = args["steps"].as_i64().unwrap_or(10).max(1) as i32;
    let button_str = args["button"].as_str().unwrap_or("left");
    let button = match button_str {
        "right" => MouseButton::Right,
        "middle" => MouseButton::Middle,
        _ => MouseButton::Left,
    };

    let mut enigo = Enigo::new();
    enigo.mouse_move_to(start_x, start_y);
    std::thread::sleep(Duration::from_millis(20));
    enigo.mouse_down(button).map_err(|e| format!("mouse_down failed: {}", e))?;
    std::thread::sleep(Duration::from_millis(20));

    for step in 1..=steps {
        let t = step as f64 / steps as f64;
        let ix = start_x as f64 + (end_x - start_x) as f64 * t;
        let iy = start_y as f64 + (end_y - start_y) as f64 * t;
        enigo.mouse_move_to(ix as i32, iy as i32);
        std::thread::sleep(Duration::from_millis(20));
    }

    enigo.mouse_up(button);

    Ok(json!([{ "type": "text", "text": format!("Dragged from ({}, {}) to ({}, {}) in {} steps", start_x, start_y, end_x, end_y, steps) }]))
}

fn tool_wait_for_screen_change(args: &Value) -> Result<Value, String> {
    let region = parse_crop_region(args);
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(10000).min(60000);
    let threshold = args["threshold"].as_f64().unwrap_or(1.0);

    let (baseline, _, _) = capture_rgba(region)?;
    let start = Instant::now();

    loop {
        if start.elapsed().as_millis() as u64 >= timeout_ms {
            let result = json!({ "changed": false, "elapsed_ms": start.elapsed().as_millis() as u64, "status": "timeout" });
            return Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]));
        }
        std::thread::sleep(Duration::from_millis(200));
        if let Ok((current, _, _)) = capture_rgba(region) {
            let diff = pixel_diff_percentage(&baseline, &current, 10);
            if diff >= threshold {
                let result = json!({ "changed": true, "elapsed_ms": start.elapsed().as_millis() as u64, "changed_percentage": (diff * 10.0).round() / 10.0 });
                return Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]));
            }
        }
    }
}

fn tool_screen_diff_summary(args: &Value) -> Result<Value, String> {
    cleanup_diff_refs();

    let reference_id = args["reference_id"].as_str();
    let region = parse_crop_region(args);

    match reference_id {
        None => {

            let (rgba, w, h) = capture_rgba(region)?;
            let id = format!("{:x}", std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
            if let Ok(mut refs) = DIFF_REFS.lock() {
                refs.insert(id.clone(), (rgba, w, h, Instant::now()));
            }
            let result = json!({ "reference_id": id, "status": "baseline_captured", "width": w, "height": h });
            Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]))
        }
        Some(ref_id) => {

            let baseline = {
                let refs = DIFF_REFS.lock().map_err(|_| "Lock error")?;
                refs.get(ref_id).map(|(rgba, w, h, _)| (rgba.clone(), *w, *h))
            };
            let (base_rgba, base_w, base_h) = baseline.ok_or_else(|| format!("Reference '{}' not found or expired", ref_id))?;
            let (current_rgba, cur_w, cur_h) = capture_rgba(region)?;

            if base_w != cur_w || base_h != cur_h {
                return Err("Screen dimensions changed since baseline capture".to_string());
            }

            let changed_pct = pixel_diff_percentage(&base_rgba, &current_rgba, 10);
            let changed_regions = grid_diff(&base_rgba, &current_rgba, base_w, base_h);

            if let Ok(mut refs) = DIFF_REFS.lock() {
                refs.remove(ref_id);
            }

            let result = json!({
                "changed_percentage": (changed_pct * 10.0).round() / 10.0,
                "changed": changed_pct > 0.5,
                "changed_regions": changed_regions,
            });
            Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]))
        }
    }
}

fn tool_verify_action_result(args: &Value) -> Result<Value, String> {
    let region = parse_crop_region(args);
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(3000).min(30000);
    let threshold = args["threshold"].as_f64().unwrap_or(1.0);

    let (baseline, w, h) = capture_rgba(region)?;
    let start = Instant::now();

    loop {
        if start.elapsed().as_millis() as u64 >= timeout_ms {
            let result = json!({ "changed": false, "elapsed_ms": start.elapsed().as_millis() as u64, "changed_percentage": 0.0, "status": "no_change_detected" });
            return Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]));
        }
        std::thread::sleep(Duration::from_millis(200));
        if let Ok((current, _, _)) = capture_rgba(region) {
            let diff = pixel_diff_percentage(&baseline, &current, 10);
            if diff >= threshold {
                let regions = grid_diff(&baseline, &current, w, h);
                let result = json!({
                    "changed": true,
                    "elapsed_ms": start.elapsed().as_millis() as u64,
                    "changed_percentage": (diff * 10.0).round() / 10.0,
                    "changed_regions": regions,
                    "status": "change_detected",
                });
                return Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]));
            }
        }
    }
}

fn tool_start_recording() -> Result<Value, String> {
    let mut state = WORKFLOW_STATE.lock().map_err(|e| format!("lock error: {}", e))?;
    state.recording = true;
    state.steps.clear();
    state.start_time = Some(Instant::now());
    Ok(json!([{ "type": "text", "text": "Recording started. All tool calls will be captured. Use stop_recording to finish." }]))
}

fn tool_stop_recording() -> Result<Value, String> {
    let mut state = WORKFLOW_STATE.lock().map_err(|e| format!("lock error: {}", e))?;
    state.recording = false;

    let mut steps_json: Vec<Value> = Vec::new();
    let mut prev_ts = 0u64;
    for step in state.steps.drain(..) {
        let delay_ms = step.timestamp_ms.saturating_sub(prev_ts);
        prev_ts = step.timestamp_ms;
        steps_json.push(json!({
            "tool_name": step.tool_name,
            "arguments": step.arguments,
            "delay_ms": delay_ms,
            "timestamp_ms": step.timestamp_ms
        }));
    }
    let count = steps_json.len();
    state.start_time = None;
    let result = json!({ "steps": steps_json, "count": count });
    Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]))
}

fn tool_replay_workflow(args: &Value) -> Result<Value, String> {
    let steps = args["steps"].as_array().ok_or("steps array is required")?;
    let multiplier = args["delay_multiplier"].as_f64().unwrap_or(1.0);
    let mut results = Vec::new();

    for (i, step) in steps.iter().enumerate() {
        let tool_name = step["tool_name"].as_str().unwrap_or("");
        let arguments = &step["arguments"];
        let delay_ms = step["delay_ms"].as_u64().unwrap_or(0);

        if matches!(tool_name, "start_recording" | "stop_recording" | "replay_workflow") {
            continue;
        }

        if i > 0 && delay_ms > 0 && multiplier > 0.0 {
            let actual_delay = (delay_ms as f64 * multiplier) as u64;
            std::thread::sleep(Duration::from_millis(actual_delay));
        }

        let fake_request = json!({
            "params": { "name": tool_name, "arguments": arguments }
        });
        let response = handle_tools_call(&fake_request);
        let is_error = response["result"]["isError"].as_bool().unwrap_or(false);
        results.push(json!({
            "step": i,
            "tool_name": tool_name,
            "success": !is_error
        }));
    }

    let result = json!({ "replayed": results.len(), "results": results });
    Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]))
}

fn tool_read_screen_text(args: &Value) -> Result<Value, String> {
    let region = parse_crop_region(args);

    let language = args["language"].as_str().map(|s| s.to_string());
    let (rgba, w, h) = capture_rgba(region)?;
    let text = run_ocr(&rgba, w, h, language.as_deref())?;

    Ok(json!([{ "type": "text", "text": text }]))
}

#[cfg(target_os = "windows")]
use crate::ocr_winrt::{run_ocr, run_ocr_with_boxes};

#[cfg(target_os = "macos")]
fn vision_class(name: &str) -> Result<&'static objc::runtime::Class, String> {
    objc::runtime::Class::get(name)
        .ok_or_else(|| format!("{} is unavailable on this version of macOS", name))
}

#[cfg(target_os = "macos")]
fn run_ocr(rgba: &[u8], width: u32, height: u32, language: Option<&str>) -> Result<String, String> {
    use objc::{class, msg_send, sel, sel_impl, runtime::Object};
    use core_graphics::color_space::CGColorSpace;
    use core_graphics::data_provider::CGDataProvider;
    use core_graphics::image::CGImage;
    use foreign_types::ForeignType;
    use std::os::raw::c_char;
    use std::sync::Arc;

    let vn_handler_class = vision_class("VNImageRequestHandler")?;
    let vn_request_class = vision_class("VNRecognizeTextRequest")?;

    unsafe {
        let pool: *mut Object = msg_send![class!(NSAutoreleasePool), new];

        let color_space = CGColorSpace::create_device_rgb();
        let rgba_owned: Arc<Vec<u8>> = Arc::new(rgba.to_vec());
        let provider = CGDataProvider::from_buffer(rgba_owned);
        let bytes_per_row = width as usize * 4;
        let cg_image = CGImage::new(
            width as usize,
            height as usize,
            8,
            32,
            bytes_per_row,
            &color_space,
            core_graphics::base::kCGBitmapByteOrderDefault | core_graphics::base::kCGImageAlphaLast,
            &provider,
            false,
            0,
        );

        let empty_dict: *mut Object = msg_send![class!(NSDictionary), dictionary];
        let handler: *mut Object = {
            let alloc: *mut Object = msg_send![vn_handler_class, alloc];
            msg_send![alloc, initWithCGImage:cg_image.as_ptr() options:empty_dict]
        };
        if handler.is_null() {
            let _: () = msg_send![pool, drain];
            return Err("Failed to create VNImageRequestHandler".to_string());
        }

        let request: *mut Object = {
            let alloc: *mut Object = msg_send![vn_request_class, alloc];
            msg_send![alloc, init]
        };
        if request.is_null() {
            let _: () = msg_send![pool, drain];
            return Err("Failed to create VNRecognizeTextRequest".to_string());
        }

        let _: () = msg_send![request, setRecognitionLevel: 0i64];

        if let Some(lang) = language {
            let ns_lang: *mut Object = {
                let s: *mut Object = msg_send![class!(NSString), alloc];
                let bytes = lang.as_bytes();
                msg_send![s, initWithBytes:bytes.as_ptr() length:bytes.len() encoding:4u64]
            };
            let lang_array: *mut Object = msg_send![class!(NSArray), arrayWithObject: ns_lang];
            let _: () = msg_send![request, setRecognitionLanguages: lang_array];
        }

        let requests: *mut Object = msg_send![class!(NSArray), arrayWithObject: request];
        let mut error: *mut Object = std::ptr::null_mut();
        let ok: bool = msg_send![handler, performRequests:requests error:&mut error];
        if !ok {
            let err_msg = if !error.is_null() {
                let desc: *mut Object = msg_send![error, localizedDescription];
                let cstr: *const c_char = msg_send![desc, UTF8String];
                std::ffi::CStr::from_ptr(cstr).to_string_lossy().to_string()
            } else {
                "Unknown error".to_string()
            };
            let _: () = msg_send![pool, drain];
            return Err(format!("Vision OCR failed: {}", err_msg));
        }

        let results: *mut Object = msg_send![request, results];
        let text = if results.is_null() {
            String::new()
        } else {
            let count: usize = msg_send![results, count];
            let mut lines = Vec::with_capacity(count);
            for i in 0..count {
                let obs: *mut Object = msg_send![results, objectAtIndex: i];
                let candidates: *mut Object = msg_send![obs, topCandidates: 1usize];
                let cand_count: usize = msg_send![candidates, count];
                if cand_count > 0 {
                    let top: *mut Object = msg_send![candidates, objectAtIndex: 0usize];
                    let ns_string: *mut Object = msg_send![top, string];
                    if !ns_string.is_null() {
                        let cstr: *const c_char = msg_send![ns_string, UTF8String];
                        if !cstr.is_null() {
                            lines.push(std::ffi::CStr::from_ptr(cstr).to_string_lossy().to_string());
                        }
                    }
                }
            }
            lines.join("\n")
        };

        let _: () = msg_send![pool, drain];
        Ok(text)
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn run_ocr(_rgba: &[u8], _width: u32, _height: u32, _language: Option<&str>) -> Result<String, String> {
    Err("OCR is not supported on this platform.".to_string())
}

pub(crate) struct OcrTextBlock {
    pub(crate) text: String,
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

#[cfg(target_os = "macos")]
fn run_ocr_with_boxes(rgba: &[u8], width: u32, height: u32) -> Result<Vec<OcrTextBlock>, String> {
    use objc::{class, msg_send, sel, sel_impl, runtime::Object};
    use core_graphics::color_space::CGColorSpace;
    use core_graphics::data_provider::CGDataProvider;
    use core_graphics::image::CGImage;
    use foreign_types::ForeignType;
    use std::os::raw::c_char;
    use std::sync::Arc;

    let vn_handler_class = vision_class("VNImageRequestHandler")?;
    let vn_request_class = vision_class("VNRecognizeTextRequest")?;

    unsafe {
        let pool: *mut Object = msg_send![class!(NSAutoreleasePool), new];

        let color_space = CGColorSpace::create_device_rgb();
        let rgba_owned: Arc<Vec<u8>> = Arc::new(rgba.to_vec());
        let provider = CGDataProvider::from_buffer(rgba_owned);
        let bytes_per_row = width as usize * 4;
        let cg_image = CGImage::new(
            width as usize, height as usize, 8, 32, bytes_per_row,
            &color_space,
            core_graphics::base::kCGBitmapByteOrderDefault | core_graphics::base::kCGImageAlphaLast,
            &provider, false, 0,
        );

        let empty_dict: *mut Object = msg_send![class!(NSDictionary), dictionary];
        let handler: *mut Object = {
            let alloc: *mut Object = msg_send![vn_handler_class, alloc];
            msg_send![alloc, initWithCGImage:cg_image.as_ptr() options:empty_dict]
        };
        if handler.is_null() {
            let _: () = msg_send![pool, drain];
            return Err("Failed to create VNImageRequestHandler".to_string());
        }

        let request: *mut Object = {
            let alloc: *mut Object = msg_send![vn_request_class, alloc];
            msg_send![alloc, init]
        };
        if request.is_null() {
            let _: () = msg_send![pool, drain];
            return Err("Failed to create VNRecognizeTextRequest".to_string());
        }
        let _: () = msg_send![request, setRecognitionLevel: 0i64];

        let requests: *mut Object = msg_send![class!(NSArray), arrayWithObject: request];
        let mut error: *mut Object = std::ptr::null_mut();
        let ok: bool = msg_send![handler, performRequests:requests error:&mut error];
        if !ok {
            let err_msg = if !error.is_null() {
                let desc: *mut Object = msg_send![error, localizedDescription];
                let cstr: *const c_char = msg_send![desc, UTF8String];
                std::ffi::CStr::from_ptr(cstr).to_string_lossy().to_string()
            } else {
                "Unknown error".to_string()
            };
            let _: () = msg_send![pool, drain];
            return Err(format!("Vision OCR failed: {}", err_msg));
        }

        let results: *mut Object = msg_send![request, results];
        let mut blocks = Vec::new();
        if !results.is_null() {
            let count: usize = msg_send![results, count];
            let img_w = width as f64;
            let img_h = height as f64;
            for i in 0..count {
                let obs: *mut Object = msg_send![results, objectAtIndex: i];
                let candidates: *mut Object = msg_send![obs, topCandidates: 1usize];
                let cand_count: usize = msg_send![candidates, count];
                if cand_count > 0 {
                    let top: *mut Object = msg_send![candidates, objectAtIndex: 0usize];
                    let ns_string: *mut Object = msg_send![top, string];
                    if !ns_string.is_null() {
                        let cstr: *const c_char = msg_send![ns_string, UTF8String];
                        if !cstr.is_null() {
                            let text = std::ffi::CStr::from_ptr(cstr).to_string_lossy().to_string();

                            #[repr(C)]
                            struct CGRect { x: f64, y: f64, w: f64, h: f64 }
                            let bbox: CGRect = msg_send![obs, boundingBox];
                            let px = (bbox.x * img_w) as i32;
                            let py = ((1.0 - bbox.y - bbox.h) * img_h) as i32;
                            let pw = (bbox.w * img_w) as i32;
                            let ph = (bbox.h * img_h) as i32;
                            blocks.push(OcrTextBlock { text, x: px, y: py, width: pw, height: ph });
                        }
                    }
                }
            }
        }

        let _: () = msg_send![pool, drain];
        Ok(blocks)
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn run_ocr_with_boxes(_rgba: &[u8], _width: u32, _height: u32) -> Result<Vec<OcrTextBlock>, String> {
    Err("OCR is not supported in this build.".to_string())
}

fn tool_get_screen_size() -> Result<Value, String> {
    #[cfg(windows)]
    {
        #[allow(clashing_extern_declarations)]
        extern "system" {
            fn GetSystemMetrics(idx: i32) -> i32;
        }
        let w = unsafe { GetSystemMetrics(0) };
        let h = unsafe { GetSystemMetrics(1) };
        if w > 0 && h > 0 {
            let result = json!({ "width": w, "height": h });
            return Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]));
        }
    }

    {
        use scrap::Display;
        let displays = Display::all().map_err(|e| format!("Failed to get displays: {}", e))?;
        if displays.is_empty() {
            return Err("No display found".to_string());
        }
        let d = &displays[0];
        let result = json!({ "width": d.width(), "height": d.height() });
        Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]))
    }
}

fn parse_enigo_key(key_str: &str) -> Result<enigo::Key, String> {
    use enigo::Key;
    match key_str.to_lowercase().as_str() {
        "ctrl" | "control" => Ok(Key::Control),
        "alt" => Ok(Key::Alt),
        "shift" => Ok(Key::Shift),
        "meta" | "super" | "win" | "cmd" => Ok(Key::Meta),
        "return" | "enter" => Ok(Key::Return),
        "escape" | "esc" => Ok(Key::Escape),
        "tab" => Ok(Key::Tab),
        "backspace" => Ok(Key::Backspace),
        "delete" => Ok(Key::Delete),
        "up" => Ok(Key::UpArrow),
        "down" => Ok(Key::DownArrow),
        "left" => Ok(Key::LeftArrow),
        "right" => Ok(Key::RightArrow),
        "home" => Ok(Key::Home),
        "end" => Ok(Key::End),
        "pageup" => Ok(Key::PageUp),
        "pagedown" => Ok(Key::PageDown),
        "space" => Ok(Key::Space),
        "f1" => Ok(Key::F1), "f2" => Ok(Key::F2), "f3" => Ok(Key::F3),
        "f4" => Ok(Key::F4), "f5" => Ok(Key::F5), "f6" => Ok(Key::F6),
        "f7" => Ok(Key::F7), "f8" => Ok(Key::F8), "f9" => Ok(Key::F9),
        "f10" => Ok(Key::F10), "f11" => Ok(Key::F11), "f12" => Ok(Key::F12),
        s if s.len() == 1 => Ok(Key::Layout(s.chars().next().unwrap())),
        _ => Err(format!("Unknown key: {}", key_str)),
    }
}

fn tool_key_hold(args: &Value) -> Result<Value, String> {
    use enigo::{Enigo, KeyboardControllable};
    let key_str = args["key"].as_str().ok_or("Missing key")?;
    let key = parse_enigo_key(key_str)?;
    let mut enigo = Enigo::new();
    let _ = enigo.key_down(key);
    Ok(json!([{ "type": "text", "text": format!("Holding {}", key_str) }]))
}

fn tool_key_release(args: &Value) -> Result<Value, String> {
    use enigo::{Enigo, KeyboardControllable};
    let key_str = args["key"].as_str().ok_or("Missing key")?;
    let key = parse_enigo_key(key_str)?;
    let mut enigo = Enigo::new();
    enigo.key_up(key);
    Ok(json!([{ "type": "text", "text": format!("Released {}", key_str) }]))
}

fn find_elements_on_screen(text: &str, exact: bool) -> Result<Vec<OcrTextBlock>, String> {
    let (rgba, w, h) = capture_rgba(None)?;
    let blocks = run_ocr_with_boxes(&rgba, w, h)?;
    let query = text.to_lowercase();
    let matched: Vec<OcrTextBlock> = blocks.into_iter().filter(|b| {
        if exact {
            b.text.to_lowercase() == query
        } else {
            b.text.to_lowercase().contains(&query)
        }
    }).collect();
    Ok(matched)
}

fn tool_find_element(args: &Value) -> Result<Value, String> {
    let text = args["text"].as_str().ok_or("Missing text")?;
    let exact = args["exact"].as_bool().unwrap_or(false);

    let matched = find_elements_on_screen(text, exact)?;

    if matched.is_empty() {
        return Ok(json!([{ "type": "text", "text": format!("No elements found matching \"{}\"", text) }]));
    }

    let results: Vec<Value> = matched.iter().map(|b| {
        json!({
            "text": b.text,
            "x": b.x, "y": b.y,
            "width": b.width, "height": b.height,
            "center_x": b.x + b.width / 2,
            "center_y": b.y + b.height / 2,
        })
    }).collect();

    Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&json!({ "matches": results })).unwrap_or_default() }]))
}

fn tool_click_text(args: &Value) -> Result<Value, String> {
    use enigo::{Enigo, MouseButton, MouseControllable};

    let text = args["text"].as_str().ok_or("Missing text")?;
    let button_str = args["button"].as_str().unwrap_or("left");
    let occurrence = args["occurrence"].as_i64().unwrap_or(1).max(1) as usize;

    let matched = find_elements_on_screen(text, false)?;

    if matched.is_empty() {
        return Err(format!("Text \"{}\" not found on screen", text));
    }
    if occurrence > matched.len() {
        return Err(format!("Only {} occurrences found, requested #{}", matched.len(), occurrence));
    }

    let target = &matched[occurrence - 1];
    let cx = target.x + target.width / 2;
    let cy = target.y + target.height / 2;

    let button = match button_str {
        "right" => MouseButton::Right,
        "middle" => MouseButton::Middle,
        _ => MouseButton::Left,
    };

    let mut enigo = Enigo::new();
    enigo.mouse_move_to(cx, cy);
    std::thread::sleep(Duration::from_millis(10));
    enigo.mouse_click(button);

    let result = json!({ "clicked": true, "text": target.text, "x": cx, "y": cy });
    Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]))
}

fn tool_wait_for_text(args: &Value) -> Result<Value, String> {
    let text = args["text"].as_str().ok_or("Missing text")?;
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(10000).min(60000);
    let interval_ms = args["interval_ms"].as_u64().unwrap_or(500).max(200);

    let query = text.to_lowercase();
    let start = std::time::Instant::now();

    loop {
        if start.elapsed().as_millis() as u64 >= timeout_ms {
            let result = json!({ "found": false, "elapsed_ms": start.elapsed().as_millis() as u64, "status": "timeout" });
            return Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]));
        }

        if let Ok((rgba, w, h)) = capture_rgba(None) {
            if let Ok(ocr_text) = run_ocr(&rgba, w, h, None) {
                if ocr_text.to_lowercase().contains(&query) {
                    let result = json!({ "found": true, "elapsed_ms": start.elapsed().as_millis() as u64 });
                    return Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]));
                }
            }
        }

        std::thread::sleep(Duration::from_millis(interval_ms));
    }
}
