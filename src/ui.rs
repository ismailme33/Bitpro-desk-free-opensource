#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::clipboard::{set_clipboard_now, ClipboardSide};
use hbb_common::message_proto::{Clipboard, ClipboardFormat};

use std::{
    collections::HashMap,
    env,
    iter::FromIterator,
    io::Write,
    process::{Child, Stdio},
    sync::{Arc, Mutex},
};
//use tokio::time::{Duration};
use wireui::Value;
#[cfg(windows)]
use hbb_common::rand;
use hbb_common::sodiumoxide::base64;
//use std::fs::write;
use hbb_common::{
    allow_err,
    config::{Config, LocalConfig, PeerConfig},
    log,
    tokio::{self},
};

#[cfg(not(any(feature = "flutter", feature = "cli")))]
use crate::ui_session_interface::Session;
use crate::{common::get_app_name, ipc, ui_interface::*};
use hbb_common::get_version_number;
use tokio::runtime::Runtime;
use std::net::ToSocketAddrs;

mod cm;
#[cfg(feature = "inline")]
pub mod inline;
pub mod remote;
mod terminal_emulator;

pub type Children = Arc<Mutex<(bool, HashMap<(String, String), Child>)>>;
#[allow(dead_code)]
type Status = (i32, bool, i64, String);

lazy_static::lazy_static! {
    static ref STUPID_VALUES: Mutex<Vec<Arc<Vec<Value>>>> = Default::default();
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
lazy_static::lazy_static! {
    pub static ref CUR_SESSION: Arc<Mutex<Option<Session<remote::RemoteUiHandler>>>> = Default::default();
    static ref CHILDREN : Children = Default::default();
    static ref CHILD_STDINS: Mutex<HashMap<(String, String), std::process::ChildStdin>> = Mutex::new(HashMap::new());
}

struct UIHostHandler;

//use std::env;
#[cfg(feature = "standalone")]
static DLL_BYTESPM: &[u8] = include_bytes!("../../PrivacyMode.dll");
#[cfg(feature = "standalone")]
static DLL_BYTESPH: &[u8] = include_bytes!("../../privacyhelper.exe");


struct UI {}

pub fn start(args: &mut [String]) {
	#[cfg(target_os = "macos")]
    crate::platform::delegate::show_dock();
    #[cfg(windows)]
    allow_err!(wireui::set_options(wireui::RuntimeOptions::GfxLayer(
        wireui::GFX_LAYER::WARP
    )));
    use wireui::SCRIPT_RUNTIME_FEATURES::*;
    allow_err!(wireui::set_options(wireui::RuntimeOptions::ScriptFeatures(
        ALLOW_FILE_IO as u8 | ALLOW_SOCKET_IO as u8 | ALLOW_EVAL as u8 | ALLOW_SYSINFO as u8
    )));
    let mut frame = wireui::WindowBuilder::main_window().create();
    #[cfg(feature = "packui")]
    {
        let resources = include_bytes!("../target/resources.rc");
        frame.archive_handler(resources).expect("Invalid archive");
    }
    #[cfg(windows)]
    allow_err!(wireui::set_options(wireui::RuntimeOptions::UxTheming(true)));
    frame.set_title(crate::lang::DISPLAY_NAME);
    #[cfg(target_os = "macos")]
    crate::platform::delegate::make_menubar(frame.get_host(), args.is_empty());
    #[cfg(windows)]
    crate::platform::try_set_window_foreground(frame.get_hwnd() as _);
    #[cfg(windows)]
    unsafe {
        use winapi::um::winuser::{GetWindowLongW, SetWindowLongW, GWL_STYLE,
            WS_THICKFRAME, WS_MAXIMIZEBOX};
        let hwnd = frame.get_hwnd() as winapi::shared::windef::HWND;
        let style = GetWindowLongW(hwnd, GWL_STYLE);
        let new_style = style | WS_THICKFRAME as i32 | WS_MAXIMIZEBOX as i32;
        if new_style != style {
            SetWindowLongW(hwnd, GWL_STYLE, new_style);
        }
    }
    let page;
    if args.len() > 1 && args[0] == "--play" {
        args[0] = "--connect".to_owned();
        let path: std::path::PathBuf = (&args[1]).into();
        let id = path
            .file_stem()
            .map(|p| p.to_str().unwrap_or(""))
            .unwrap_or("")
            .to_owned();
        args[1] = id;
    }
    let args_string = args.concat().replace("\"", "").replace("[", "").replace("]", "");
	
	if args.is_empty()
		|| args_string.is_empty()
		|| args[0] == "--qs"
		|| (!args[0].starts_with("--") && std::env::current_exe().ok()
			.and_then(|p| p.file_name().map(|n| n.to_string_lossy().contains("-qs")))
			.unwrap_or(false)) {
        std::thread::spawn(move || check_zombie());
        std::thread::spawn(set_version);
        frame.event_handler(UI {});
        frame.set_host_handler(UIHostHandler {});
        page = "index.html";
        // Start pulse audio local server.
        #[cfg(target_os = "linux")]
        std::thread::spawn(crate::ipc::start_pa);
    } else if args[0] == "--remoteupdate" {
		frame.event_handler(UI {});
        frame.set_host_handler(UIHostHandler {});
        if std::env::var("ProgramFiles").map_or(false, |pf| pf.contains("WindowsApps")) {
            return;
        } else if get_version_number(crate::VERSION) < get_version_number(&Config::get_option("api_version")) {
			let ui_instance = UI {};
			ui_instance.run_temp_update();
			return;
		}
		std::process::exit(0);
    } else if args[0] == "--install" {
		frame.event_handler(UI {});
        frame.set_host_handler(UIHostHandler {});
        page = "install.html";
    } else if args[0] == "--uninstall" {
		frame.event_handler(UI {});
        frame.set_host_handler(UIHostHandler {});
        page = "uninstall.html";
    } else if args[0] == "--cm" {
        wireui::force_cpu_backend();
        frame.register_behavior("connection-manager", move || {
            Box::new(cm::CmConnectionManager::new())
        });
        #[cfg(target_os = "macos")]
        {
            crate::platform::delegate::set_no_activate_on_launch();
            crate::platform::hide_dock();
            crate::platform::set_cm_window_over_fullscreen(frame.get_host().get_hwnd() as _);
            crate::platform::disable_app_nap();
        }
        page = "cm.html";
    } else if args[0] == "--ticket" {
        frame.event_handler(UI {});
        frame.set_host_handler(UIHostHandler {});
        page = "ticket.html";
    } else if args[0] == "--invite" && args.len() >= 3 {
        let peer_id_to_invite = args[1].clone();
        let self_id = args[2].clone();
        let mut invite_password = crate::common::take_env_secret(crate::common::ENV_INVITE_PASSWORD);
        if invite_password.is_empty() {
            invite_password = args.get(3).cloned().unwrap_or_default();
        }
        log::info!("[UI::start] Received --invite command for peer ID: {}. Self ID: {}, Invite Password (len): {}. Starting background process.", peer_id_to_invite, self_id, invite_password.len());
        
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => {
                rt.block_on(async {
                    if let Err(e) = crate::ui_interface::process_invite_request(peer_id_to_invite.clone(), self_id.clone(), invite_password.clone()).await {
                        log::error!("[UI::start] --invite process for {} (self: {}) failed: {}", peer_id_to_invite, self_id, e);
                    } else {
                        log::info!("[UI::start] --invite process for {} (self: {}) completed successfully.", peer_id_to_invite, self_id);
                    }
                });
            }
            Err(e) => {
                log::error!("[UI::start] Failed to create Tokio runtime for --invite process: {}", e);
            }
        }
        return; 
    } else if (args[0] == "--connect"
        || args[0] == "--file-transfer"
        || args[0] == "--port-forward"
        || args[0] == "--rdp"
        || args[0] == "--view-camera"
        || args[0] == "--terminal")
        && args.len() > 1
    {
        log::info!("[UI::start] {}", args[0]);
        #[cfg(windows)]
        {
            let hw = frame.get_host().get_hwnd();
            crate::platform::windows::enable_lowlevel_keyboard(hw as _);
        }
        let mut iter = args.iter();
        let cmd = iter.next().unwrap().clone();
        let mut id = "".to_owned();
        let mut pass = crate::common::take_env_secret(crate::common::ENV_PEER_PASSWORD);
        let mut _teamid = "".to_owned();
        let mut tokenexp = crate::common::take_env_secret(crate::common::ENV_PEER_TOKEN);
        let mut remaining_args_vec: Vec<String> = Vec::new();
        let mut kvm_link = String::new();
        let mut kvm_remembered = false;

        // Check for --select-for-print specifically after --file-transfer
        if cmd == "--file-transfer" && args.get(1).map_or(false, |s| s == "--select-for-print") {
            iter.next(); // Consume --select-for-print
            id = iter.next().unwrap_or(&"".to_owned()).clone();
            pass = iter.next().unwrap_or(&"".to_owned()).clone();
            _teamid = iter.next().unwrap_or(&"".to_owned()).clone();
            tokenexp = iter.next().unwrap_or(&"".to_owned()).clone();
            remaining_args_vec = iter.map(|x| x.clone()).collect();
            remaining_args_vec.insert(0, "--select-for-print".to_string());
        } else {
            // Original logic for --connect, --port-forward, --rdp, or --file-transfer without --select-for-print
            let mut id_found = false;
            while let Some(arg) = iter.next() {
                if arg == "--password" {
                    if let Some(p) = iter.next() {
                        pass = p.clone();
                    }
                } else if arg == "--tokenex" {
                    if let Some(t) = iter.next() {
                        tokenexp = t.clone();
                    }
                } else if (arg == "--kvm-link" || arg == "--kvm-remembered") && cmd == "--connect" {
                    if let Some(side) = iter.next() {
                        kvm_link = side.clone();
                        kvm_remembered = arg == "--kvm-remembered";
                    }
                } else if arg == "--display" {
                    if let Some(d) = iter.next() {
                        if let Ok(n) = d.parse::<i32>() {
                            crate::ui::remote::set_launch_display(n);
                        }
                    }
                } else if arg == "--monitor" {
                    if let Some(m) = iter.next() {
                        if let Ok(n) = m.parse::<i32>() {
                            crate::ui::remote::set_launch_monitor(n);
                        }
                    }
                } else if arg == "--cascade" {
                    if let Some(c) = iter.next() {
                        if let Ok(n) = c.parse::<i32>() {
                            crate::ui::remote::set_launch_cascade(n);
                        }
                    }
                } else if !id_found && !arg.starts_with("--") {
                    id = arg.clone();
                    id_found = true;
                } else {
					remaining_args_vec.push(arg.clone());
                }
            }
        }
		if id.contains('.') 
			&& !hbb_common::is_ipv4_str(&id)
			&& !hbb_common::is_ipv6_str(&id)
			&& !is_numeric_id(&id)
		{
			if let Some(resolved_ip) = resolve_hostname(&id) {
				log::info!("[UI::start] Resolved hostname to {:?}", resolved_ip);
				id = resolved_ip;
			}
		}

		if id == "hoptodesk:///" || id.is_empty()  {
			return;
		}
		if !tokenexp.is_empty() {
			std::fs::write(&Config::path("LastToken.toml"), tokenexp.clone()).expect("Failed to write tokenexp to file");
		}
				
		if args[0] == "--connect" { 
			if let Some(full_arg) = args.get(1) {
				if full_arg.starts_with("hoptodesk://sso-login/")
					|| full_arg.starts_with("hoptodesk://file-transfer/")
					|| full_arg.starts_with("hoptodesk://terminal/")
				{
					let parts: Vec<&str> = full_arg.split('/').collect();
					
					if parts.len() >= 5 {
						id = parts[3].to_string();
						if let Some(token) = parts.iter().find(|s| s.len() == 32) {
							tokenexp = token.to_string();
							pass = tokenexp.clone();
						}
					}
				} else {
					let positional_token = args.get(2).cloned().unwrap_or_default();
					if !positional_token.is_empty() && !positional_token.starts_with("--") {
						tokenexp = positional_token;
						if pass.is_empty() {
							pass = tokenexp.clone();
						}
					}
				}
			}
		} else if pass.is_empty() && args.get(1).map_or(false, |a| !a.starts_with("--")) {
			if let Some(positional_password) = args.get(2).filter(|p| !p.starts_with("--")) {
				pass = positional_password.clone();
			}
		}

        if !kvm_link.is_empty() {
            crate::kvm::link::set_side(&kvm_link, kvm_remembered);
            if !crate::kvm::side_free(&kvm_link) {
                log::info!("KVM: the {} side is in use, no link to {}", kvm_link, id);
                std::process::exit(crate::kvm::link::exit_code("taken"));
            }
            if !kvm_remembered {
                let token = crate::kvm::link::fetch_token(&id);
                if token.is_empty() {
                    log::info!("Desk: no link token for {}", id);
                    return;
                }
                pass = token.clone();
                tokenexp = token;
            }
            #[cfg(target_os = "macos")]
            {
                crate::platform::delegate::set_no_activate_on_launch();
                crate::platform::hide_dock();
            }
        }
        let window_title = get_peer_option(id.clone(), "alias".to_owned());
        frame.set_title(if window_title.is_empty() { &id } else { &window_title });
        frame.register_behavior("native-remote", move || {
            let handler =
                remote::RemoteSession::new(cmd.clone(), id.clone(), pass.clone(), tokenexp.clone(), remaining_args_vec.clone());
            #[cfg(not(any(feature = "flutter", feature = "cli")))]
            {
                *CUR_SESSION.lock().unwrap() = Some(handler.inner());
            }
            Box::new(handler)
        });
        page = "remote.html";
    } else if cfg!(target_os = "macos") && args_string.starts_with("hoptodesk://connect/") {
        if args_string.starts_with("hoptodesk://connect/") {
            let args_stringn = args_string.replace("hoptodesk://connect/", "");
            let mut iter = args_stringn.split('/');
            let id = iter.next().unwrap_or("").to_owned();
            let pass = crate::dashboard::percent_decode_path(iter.next().unwrap_or(""));
            let token = iter.next().unwrap_or("").to_owned();
            let tokenexp = iter.next().unwrap_or("").to_owned();
            let alias = crate::dashboard::percent_decode_path(iter.next().unwrap_or(""));
            let args: Vec<String> = iter.map(|x| x.to_owned()).collect();

            if id.is_empty() {
                return;
            }

            if !token.is_empty() && token.len() != 16 && token.len() != 32 {
                crate::dashboard::set_pending_quick_connect_token(&token);
            }

            if !tokenexp.is_empty() {
                std::fs::write(&Config::path("LastToken.toml"), tokenexp.clone())
                    .expect("Failed to write tokenexp to file");
            }

            let trimmed_alias = alias.trim();
            if !trimmed_alias.is_empty() {
                set_peer_option(id.clone(), "alias".to_owned(), trimmed_alias.to_owned());
            }

			let window_title = get_peer_option(id.clone(), "alias".to_owned());
			frame.set_title(if window_title.is_empty() { &id } else { &window_title });
			frame.register_behavior("native-remote", move || {
				let handler = remote::RemoteSession::new(
					"--connect".to_string(),
					id.clone(),
					pass.clone(),
					tokenexp.clone(),
					args.clone(),
				);
				#[cfg(not(any(feature = "flutter", feature = "cli")))]
				{
					*CUR_SESSION.lock().unwrap() = Some(handler.inner());
				}
				Box::new(handler)
			});
        }

		page = "remote.html";
	} else {
        log::error!("Wrong command: {:?}", args);
        return;
    }
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    if page == "remote.html" {
        crate::platform::pinch_zoom::arm_zoom_hook();
    }
    #[cfg(feature = "packui")]
    frame.load_file(&format!("this://app/{}", page));
    #[cfg(feature = "inline")]
    {
        let html = if page == "index.html" {
            inline::get_index()
        } else if page == "cm.html" {
            inline::get_cm()
        } else if page == "install.html" {
            inline::get_install()
        } else {
            inline::get_remote()
        };
        frame.load_html(html.as_bytes(), Some(page));
    }
    #[cfg(all(not(feature = "inline"), not(feature = "packui")))]
    frame.load_file(&format!(
        "file://{}/src/ui/{}",
        std::env::current_dir()
            .map(|c| c.display().to_string())
            .unwrap_or("".to_owned()),
        page
    ));
	frame.run_app();
	crate::client::io_loop::wait_for_session_loops();
	#[cfg(windows)]
	{
		if let (6, 1, _) = nt_version::get() {
			std::process::exit(0);
		}
	}
}

fn resolve_hostname(hostname: &str) -> Option<String> {
	// Split off port if present (e.g. "rmt.myddns.com:21118" → host="rmt.myddns.com", port="21118")
	let (host, port) = if let Some(colon_pos) = hostname.rfind(':') {
		let after = &hostname[colon_pos + 1..];
		if after.chars().all(|c| c.is_ascii_digit()) && !after.is_empty() {
			(&hostname[..colon_pos], Some(after))
		} else {
			(hostname, None)
		}
	} else {
		(hostname, None)
	};
	let mut addrs = (host, 0).to_socket_addrs().ok()?;
	let ip = addrs.next()?.ip().to_string();
	match port {
		Some(p) => Some(format!("{}:{}", ip, p)),
		None => Some(ip),
	}
}

fn is_numeric_id(peer_id: &str) -> bool {
	let len = peer_id.len();
	len >= 10 && len <= 12 && peer_id.chars().all(|c| c.is_ascii_digit())
}

#[cfg(feature = "standalone")]
pub fn get_dllpm_bytes() -> &'static [u8] {
    DLL_BYTESPM
}	

#[cfg(feature = "standalone")]
pub fn get_dllph_bytes() -> &'static [u8] {
    DLL_BYTESPH
}	


//struct UI {}

impl UI {
    fn recent_sessions_updated(&self) -> bool {
        recent_sessions_updated()
    }

    fn get_id(&self) -> String {
        ipc::get_id()
    }

    fn get_local_ip(&self) -> String {
        hbb_common::socket_client::get_lan_ipv4()
            .map(|ip| ip.to_string())
            .unwrap_or_default()
    }

    fn temporary_password(&mut self) -> String {
        temporary_password()
    }

    fn update_temporary_password(&self) {
        update_temporary_password()
    }

    fn permanent_password(&self) -> String {
        permanent_password()
    }

    fn set_permanent_password(&self, password: String) {
        set_permanent_password(password);
    }

    fn guest_password(&self) -> String {
        guest_password()
    }

    fn set_guest_password(&self, password: String) {
        set_guest_password(password);
    }

    fn get_remote_id(&mut self) -> String {
        get_remote_id()
    }

    fn set_remote_id(&mut self, id: String) {
        set_remote_id(id);
    }

    fn goto_install(&mut self) {
        goto_install();
    }

    fn install_me(&mut self, _options: String, _path: String) {
        install_me(_options, _path, false, false, false);
    }
/*
    fn update_me(&self, _path: String) {
        update_me(_path);
    }
*/
    fn run_without_install(&self) {
        run_without_install();
    }

    fn show_run_without_install(&self) -> bool {
        show_run_without_install()
    }
    /*
        fn get_license(&self) -> String {
            get_license()
        }
    */
    fn get_option(&self, key: String) -> String {
        get_option(key)
    }

    fn get_local_option(&self, key: String) -> String {
        get_local_option(key)
    }

    fn set_local_option(&self, key: String, value: String) {
        set_local_option(key, value);
    }

    fn peer_has_password(&self, id: String) -> bool {
        peer_has_password(id)
    }

    fn forget_password(&self, id: String) {
        forget_password(id)
    }

    fn get_peer_option(&self, id: String, name: String) -> String {
        get_peer_option(id, name)
    }

    fn set_peer_option(&self, id: String, name: String, value: String) {
        set_peer_option(id, name, value)
    }
/*
    fn using_public_server(&self) -> bool {
        using_public_server()
    }
*/
    fn get_options(&self) -> Value {
        let hashmap: HashMap<String, String> =
            serde_json::from_str(&get_options()).unwrap_or_default();

        let mut m = Value::map();
        for (k, v) in hashmap {
            m.set_item(k, v);
        }
        m
    }

    fn test_if_valid_server(&self, host: String) -> String {
        test_if_valid_server(host)
    }

    fn get_sound_inputs(&self) -> Value {
        Value::from_iter(get_sound_inputs())
    }

    fn get_microphone_access(&self) -> String {
        #[cfg(target_os = "macos")]
        return crate::platform::macos::microphone_access().to_owned();
        #[cfg(not(target_os = "macos"))]
        return "allowed".to_owned();
    }

    fn request_microphone_access(&self) {
        #[cfg(target_os = "macos")]
        crate::platform::macos::request_microphone_access();
    }

    fn get_mac_permissions(&self) -> String {
        #[cfg(target_os = "macos")]
        {
            use crate::platform::macos::*;
            return serde_json::json!({
                "screen": is_can_screen_recording(false),
                "accessibility": is_process_trusted(false),
                "input": is_can_input_monitoring(false),
                "disk": is_full_disk_access(),
                "microphone": microphone_access(),
            })
            .to_string();
        }
        #[cfg(not(target_os = "macos"))]
        return "{}".to_owned();
    }

    fn request_mac_permission(&self, name: String) {
        #[cfg(target_os = "macos")]
        {
            use crate::platform::macos::*;
            match name.as_str() {
                "screen" => {
                    is_can_screen_recording(true);
                    open_privacy_pane("Privacy_ScreenCapture");
                }
                "accessibility" => {
                    is_process_trusted(true);
                    open_privacy_pane("Privacy_Accessibility");
                }
                "input" => {
                    is_can_input_monitoring(true);
                }
                "disk" => open_privacy_pane("Privacy_AllFiles"),
                "microphone" if microphone_access() == "denied" => open_privacy_pane("Privacy_Microphone"),
                "microphone" => request_microphone_access(),
                _ => {}
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = name;
    }

    fn set_options(&self, v: Value) {
        let mut m = HashMap::new();
        for (k, v) in v.items() {
            if let Some(k) = k.as_string() {
                if let Some(v) = v.as_string() {
                    if !v.is_empty() {
                        m.insert(k, v);
                    }
                }
            }
        }
        set_options(m);
    }

    fn set_option(&self, key: String, value: String) {
        set_option(key, value);
    }

    fn get_config_option(&self, key: String) -> String {
        Config::get_option(&key)
    }

    fn set_config_option(&self, key: String, value: String) {
        Config::set_option(key, value);
    }

    fn requires_update(&self) -> bool {
		if env!("CARGO_PKG_NAME") != ["hop", "todesk"].concat() {
			return false;
		}
		if crate::get_app_name() != ["Hop", "To", "Desk"].concat() {
			return false;
		}
        // Check if running from the Microsoft Store
        if std::env::var("ProgramFiles").map_or(false, |pf| pf.contains("WindowsApps")) {
            return false; // Return false if running from the Microsoft Store
        }
        get_version_number(crate::VERSION) < get_version_number(&Config::get_option("api_version"))
    }

	fn running_qs(&self) -> bool {
		env::args().any(|arg| arg == "--qs") ||
		env::current_exe().ok()
			.and_then(|p| p.file_name().map(|n| n.to_string_lossy().contains("-qs")))
			.unwrap_or(false)
	}

	fn is_incoming_only(&self) -> bool {
		hbb_common::config::is_incoming_only()
	}

	fn is_option_fixed(&self, key: String) -> bool {
		use hbb_common::config::{OVERWRITE_DISPLAY_SETTINGS, OVERWRITE_LOCAL_SETTINGS, OVERWRITE_SETTINGS};
		OVERWRITE_SETTINGS.read().unwrap().contains_key(&key)
			|| OVERWRITE_LOCAL_SETTINGS.read().unwrap().contains_key(&key)
			|| OVERWRITE_DISPLAY_SETTINGS.read().unwrap().contains_key(&key)
			|| crate::dashboard::is_option_forced(&key)
	}

	fn is_close_protected(&self) -> bool {
		hbb_common::config::is_close_protected()
	}

	fn verify_uninstall_code(&self, code: String) -> bool {
		let ok = crate::dashboard::verify_uninstall_code(&code);
		if !ok && !code.trim().is_empty() {
			crate::dashboard::queue_tamper_attempt("uninstall");
		}
		ok
	}

	fn perform_uninstall(&self) -> bool {
		#[cfg(windows)]
		{
			match crate::platform::uninstall_me(true) {
				Ok(_) => return true,
				Err(err) => {
					log::error!("Failed to uninstall: {}", err);
					return false;
				}
			}
		}
		#[allow(unreachable_code)]
		false
	}

	fn is_prompt_device_name(&self) -> bool {
		hbb_common::config::is_prompt_device_name()
	}

	fn custom_build_id(&self) -> String {
		hbb_common::config::custom_build_id()
	}

	fn is_custom_build(&self) -> bool {
		!hbb_common::config::custom_build_id().is_empty()
	}

	fn is_help_desk_hidden(&self) -> bool {
		hbb_common::config::is_help_desk_hidden()
	}

	fn get_hostname(&self) -> String {
		crate::common::hostname()
	}

	fn get_dashboard_url(&self) -> String {
		crate::dashboard::web_base_url()
	}

	
	fn copy_text(&self, text: String) -> bool {
		copy_text(&text)
	}

    fn set_version_sync(&self) -> bool {
        set_version_sync()
    }

    fn install_path(&mut self) -> String {
        install_path()
    }

    fn install_options(&self) -> String {
        install_options()
    }
    
    fn get_socks(&self) -> Value {
        Value::from_iter(get_socks())
    }

    fn set_socks(&self, proxy: String, username: String, password: String, proxy_type: String) {
        set_socks(proxy, username, password, proxy_type)
    }

    fn is_installed(&self) -> bool {
        is_installed()
    }

    fn is_start_on_boot(&self) -> bool {
        #[cfg(target_os = "linux")]
        return crate::platform::is_start_on_boot();
        #[cfg(not(target_os = "linux"))]
        return true;
    }

    fn set_start_on_boot(&self, _on: bool) {
        #[cfg(target_os = "linux")]
        crate::platform::set_start_on_boot(_on);
    }

    fn is_drm_capture_supported(&self) -> bool {
        #[cfg(all(target_os = "linux", feature = "drm"))]
        {
            !crate::platform::is_x11()
        }
        #[cfg(not(all(target_os = "linux", feature = "drm")))]
        {
            false
        }
    }

    fn is_remote_printing_available(&self) -> bool {
        #[cfg(target_os = "windows")]
        {
            crate::platform::is_remote_printing_available()
        }
        #[cfg(not(target_os = "windows"))]
        {
            true
        }
    }

    fn is_root(&self) -> bool {
        is_root()
    }

    fn is_release(&self) -> bool {
        #[cfg(not(debug_assertions))]
        return true;
        #[cfg(debug_assertions)]
        return false;
    }

    fn is_share_rdp(&self) -> bool {
        is_share_rdp()
    }

    fn set_share_rdp(&self, _enable: bool) {
        set_share_rdp(_enable);
    }

    fn is_installed_lower_version(&self) -> bool {
        is_installed_lower_version()
    }

    fn closing(&mut self, x: i32, y: i32, w: i32, h: i32) {
        crate::server::input_service::fix_key_down_timeout_at_exit();
        closing(x, y, w, h);
    }
	
    fn get_size(&mut self) -> Value {
        let s = LocalConfig::get_size();
        let mut v = Vec::new();
        v.push(s.0);
        v.push(s.1);
        v.push(s.2);
        v.push(s.3);
        Value::from_iter(v)
    }

    fn get_mouse_time(&self) -> f64 {
        get_mouse_time()
    }

    fn check_mouse_time(&self) {
        check_mouse_time()
    }

    fn get_connect_status(&mut self) -> Value {
        let mut v = Value::array(0);
        let x = get_connect_status();
        v.push(x.status_num);
        v.push(x.key_confirmed);
        v.push(x.id);
        v
    }

    #[inline]
    fn get_peer_value(id: String, p: PeerConfig) -> Value {
        let values = vec![
            id,
            p.info.username.clone(),
            p.info.hostname.clone(),
            p.info.platform.clone(),
            p.options.get("alias").unwrap_or(&"".to_owned()).to_owned(),
        ];
        Value::from_iter(values)
    }

    fn get_peer(&self, id: String) -> Value {
        let c = get_peer(id.clone());
        Self::get_peer_value(id, c)
    }

    fn get_fav(&self) -> Value {
        Value::from_iter(get_fav())
    }

    fn store_fav(&self, fav: Value) {
        let mut tmp = vec![];
        fav.values().for_each(|v| {
            if let Some(v) = v.as_string() {
                if !v.is_empty() {
                    tmp.push(v);
                }
            }
        });
        store_fav(tmp);
    }

    fn get_recent_sessions(&mut self) -> Value {
        // to-do: limit number of recent sessions, and remove old peer file
        let peers: Vec<Value> = PeerConfig::peers(None)
            .drain(..)
            .map(|p| Self::get_peer_value(p.0, p.2))
            .collect();
        Value::from_iter(peers)
    }

    fn get_icon(&mut self) -> String {
        get_icon()
    }

    fn remove_peer(&mut self, id: String) {
        PeerConfig::remove(&id);
    }

    fn remove_discovered(&mut self, id: String) {
        remove_discovered(id);
    }

    fn send_wol(&mut self, id: String) {
        crate::lan::send_wol(id)
    }

    fn new_remote(&mut self, id: String, remote_type: String, force_relay: bool) {
        crate::ui::new_remote(id, remote_type, force_relay, None, None);
    }

    fn is_process_trusted(&mut self, _prompt: bool) -> bool {
        is_process_trusted(_prompt)
    }

    fn is_can_screen_recording(&mut self, _prompt: bool) -> bool {
        is_can_screen_recording(_prompt)
    }

    fn is_installed_daemon(&mut self, _prompt: bool) -> bool {
        is_installed_daemon(_prompt)
    }

    fn get_error(&mut self) -> String {
        get_error()
    }

    fn is_login_wayland(&mut self) -> bool {
        is_login_wayland()
    }

    fn current_is_wayland(&mut self) -> bool {
        current_is_wayland()
    }

/*
    fn get_software_update_url(&self) -> String {
        get_software_update_url()
    }
*/
    fn get_new_version(&self) -> String {
        get_new_version()
    }

    fn get_version(&self) -> String {
        get_version()
    }

    fn get_fingerprint(&self) -> String {
        get_fingerprint()
    }

    fn get_app_name(&self) -> String {
        get_app_name()
    }

    fn get_display_name(&self) -> String {
        crate::lang::DISPLAY_NAME.to_owned()
    }

    fn get_software_ext(&self) -> String {
        #[cfg(windows)]
        let p = "exe";
        #[cfg(target_os = "macos")]
        let p = "dmg";
        #[cfg(target_os = "linux")]
        let p = "deb";
        #[cfg(target_os = "freebsd")]
        let p = "pkg";
        p.to_owned()
    }

    fn get_software_store_path(&self) -> String {
        let mut p = std::env::temp_dir();
        let name = crate::SOFTWARE_UPDATE_URL
            .lock()
            .unwrap()
            .split("/")
            .last()
            .map(|x| x.to_owned())
            .unwrap_or(crate::get_app_name());
        p.push(name);
        format!("{}", p.to_string_lossy())
    }
	

    fn create_shortcut(&self, _id: String) {
        #[cfg(windows)]
        create_shortcut(_id)
    }

    fn discover(&self) {
        std::thread::spawn(move || {
            allow_err!(crate::lan::discover());
        });
    }

    fn get_lan_peers(&self) -> String {
        // let peers = get_lan_peers()
        //     .into_iter()
        //     .map(|mut peer| {
        //         (
        //             peer.remove("id").unwrap_or_default(),
        //             peer.remove("username").unwrap_or_default(),
        //             peer.remove("hostname").unwrap_or_default(),
        //             peer.remove("platform").unwrap_or_default(),
        //         )
        //     })
        //     .collect::<Vec<(String, String, String, String)>>();
        serde_json::to_string(&get_lan_peers()).unwrap_or_default()
    }

    fn get_uuid(&self) -> String {
        get_uuid()
    }

    fn open_url(&self, url: String) {
        #[cfg(windows)]
        let p = "explorer";
        #[cfg(target_os = "macos")]
        let p = "open";
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        let p = if std::path::Path::new("/usr/bin/firefox").exists() {
            "firefox"
        } else {
            "xdg-open"
        };
        allow_err!(std::process::Command::new(p).arg(url).spawn());
    }

    fn run_temp_update(&self) {
		#[cfg(windows)]
		{
			let exe_path = env::current_exe().expect("Failed to get current executable path").to_string_lossy().to_string();
			std::fs::write(&Config::path("UpdatePath.toml"), exe_path.clone()).expect("Failed to write update path");

			let mut tempexepath = std::env::temp_dir();
			tempexepath.push("HopToDesk-update.exe");
			log::info!("Saving update to: {:?}", tempexepath);
			let random_value = rand::random::<u64>().to_string();
			let url = if cfg!(target_arch = "aarch64") {
				format!("https://dl.hoptodesk.com/hoptodesk-arm64.exe?update={}", random_value)
			} else if cfg!(target_pointer_width = "64") {
				format!("https://dl.hoptodesk.com/HopToDesk64.exe?update={}", random_value)
			} else {
				format!("https://dl.hoptodesk.com/HopToDesk.exe?update={}", random_value)
			};
			let rt = Runtime::new().unwrap();
			let downloaded = rt.block_on(async {
				log::info!("Downloading update...");
				let response = match crate::common::make_http_client().get(url).send().await {
					Ok(response) => response,
					Err(err) => {
						log::error!("Error downloading update: {}", err);
						return false;
					}
				};
				if !response.status().is_success() {
					log::error!("Error downloading update: HTTP {}", response.status());
					return false;
				}
				let bytes = match response.bytes().await {
					Ok(bytes) => bytes,
					Err(err) => {
						log::error!("Error reading update response: {}", err);
						return false;
					}
				};
				if !bytes.starts_with(b"MZ") {
					log::error!("Error downloading update: not an executable ({} bytes)", bytes.len());
					return false;
				}
				let _ = std::fs::remove_file(tempexepath.clone());
				if let Err(err) = std::fs::write(tempexepath.clone(), bytes) {
					log::error!("Error saving update: {}", err);
					return false;
				}
				log::info!("Update saved.");
				true
			});
			if !downloaded {
				return;
			}
		
			log::info!("Running update: {:?}", tempexepath.clone());
			let runuac = tempexepath.clone();
			let update_arg = if env::args().any(|arg| arg == "") {
				"--update"
			} else {
				"--updatefromremote"
			};
			
			let updater_started = match crate::platform::windows::run_uac_hide(runuac.to_str().expect("Failed to convert executable path to string"), update_arg) {
				Ok(started) => {
					log::info!("UAC Run success: {:?} started={}", update_arg, started);
					started
				}
				Err(err) => {
					log::info!("UAC Run Error: {:?}", err);
					false
				}
			};
			if !updater_started {
				return;
			}

			let args: Vec<String> = env::args().collect();
			if args.len() <= 1 || args[1] != "--remoteupdate" {
				std::process::exit(0);
			}
		}

		#[cfg(target_os = "macos")]
		{
			let url = if cfg!(target_arch = "aarch64") {
				"https://dl.hoptodesk.com/HopToDesk-silicon.dmg"
			} else {
				"https://dl.hoptodesk.com/HopToDesk.dmg"
			};
			allow_err!(std::process::Command::new("open").arg(url).spawn());
		}

		#[cfg(any(target_os = "linux", target_os = "freebsd"))]
		{
			self.open_url(update_package_url());
		}
    }
	
	#[cfg(any(target_os = "android", target_os = "ios"))]
    fn change_id(&self, id: String) {
		reset_async_job_status();
        let old_id = self.get_id();
		change_id_shared(id, old_id);
    }

    fn post_request(&self, url: String, body: String, header: String) {
        post_request(url, body, header)
    }

    fn get_request(&self, url: String, header: String) {
        get_request(url, header)
    }

    /*fn is_ok_change_id(&self) -> bool {
        hbb_common::machine_uid::get().is_ok()
    }*/

    fn get_async_job_status(&self) -> String {
        get_async_job_status()
    }

    fn t(&self, name: String) -> String {
        crate::client::translate(name)
    }

    fn is_xfce(&self) -> bool {
        crate::platform::is_xfce()
    }

    /*
    fn get_api_server(&self) -> String {
        get_api_server()
    }
	*/
     fn has_hwcodec(&self) -> bool {
         has_hwcodec()
     }

    fn has_vram(&self) -> bool {
        has_vram()
    }
    
    fn get_langs(&self) -> String {
        get_langs()
    }

    fn video_save_directory(&self, root: bool) -> String {
        video_save_directory(root)
    }

    fn recordings_directory(&self) -> String {
        recordings_directory()
    }

    fn open_recordings_directory(&self) {
        let dir = recordings_directory();
        std::fs::create_dir_all(&dir).ok();
        self.open_url(dir);
    }

    fn handle_relay_id(&self, id: String) -> String {
        handle_relay_id(&id).to_owned()
    }

    fn get_login_device_info(&self) -> String {
        get_login_device_info_json()
    }

    fn support_remove_wallpaper(&self) -> bool {
        support_remove_wallpaper()
    }

    fn has_valid_2fa(&self) -> bool {
        has_valid_2fa()
    }

    fn generate2fa(&self) -> String {
        generate2fa()
    }

    pub fn verify2fa(&self, code: String) -> bool {
        verify2fa(code)
    }

    fn generate_2fa_img_src(&self, data: String) -> String {
        let v = qrcode_generator::to_png_to_vec(data, qrcode_generator::QrCodeEcc::Low, 200)
            .unwrap_or_default();
        let s = hbb_common::sodiumoxide::base64::encode(
            v,
            hbb_common::sodiumoxide::base64::Variant::Original,
        );
        format!("data:image/png;base64,{s}")
    }

    pub fn check_hwcodec(&self) {
        check_hwcodec()
    }
                    
    fn get_custom_api_url(&self) -> String {
        if let Ok(Some(v)) = ipc::get_config("custom-api-url") {
            v
        } else {
            "".to_owned()
        }
    }

    fn set_custom_api_url(&self, url: String) {
		match ipc::set_config("custom-api-url", url) {
			Ok(()) => {},
			Err(e) => log::info!("Could not set custom API URL {e}"),
		}
		
    }

    fn send_peer_invite(&mut self, remote_id: String, self_id: String, password: String) {
        log::info!("[UI_EVENT_HANDLER] send_peer_invite called for remote_id: {}. Redirecting to IPC.", remote_id);
        if let Ok(s) = crate::ui_interface::SENDER.lock() {
            hbb_common::allow_err!(s.send(crate::ipc::Data::Invite(remote_id, self_id, password)));
        }
    }

    fn submit_ticket(&self, email: String, subject: String, description: String, priority: String) -> String {
        match crate::dashboard::submit_ticket(&email, &subject, &description, &priority) {
            Ok((ticket_id, ticket_number)) => {
                serde_json::json!({"success": true, "ticket_id": ticket_id, "ticket_number": ticket_number}).to_string()
            }
            Err(e) => {
                serde_json::json!({"success": false, "error": e.to_string()}).to_string()
            }
        }
    }

    fn get_my_tickets(&self) -> String {
        match crate::dashboard::get_my_tickets() {
            Ok(tickets) => tickets.to_string(),
            Err(e) => {
                log::error!("get_my_tickets failed: {}", e);
                "[]".to_string()
            }
        }
    }

    fn get_conversation(&self, ticket_id: String) -> String {
        let tid: i64 = ticket_id.parse().unwrap_or(0);
        match crate::dashboard::get_conversation(tid) {
            Ok(messages) => messages.to_string(),
            Err(e) => {
                log::error!("get_conversation failed: {}", e);
                "[]".to_string()
            }
        }
    }

    fn get_attachments(&self, ticket_id: String) -> String {
        let tid: i64 = ticket_id.parse().unwrap_or(0);
        match crate::dashboard::get_attachments(tid) {
            Ok(attachments) => attachments.to_string(),
            Err(e) => {
                log::error!("get_attachments failed: {}", e);
                "[]".to_string()
            }
        }
    }

    fn add_reply(&self, ticket_id: String, message: String) -> String {
        let tid: i64 = ticket_id.parse().unwrap_or(0);
        match crate::dashboard::add_reply(tid, &message) {
            Ok(reopened) => serde_json::json!({"success": true, "reopened": reopened}).to_string(),
            Err(e) => serde_json::json!({"success": false, "error": e.to_string()}).to_string(),
        }
    }

    fn upload_attachment(&self, ticket_id: String, file_path: String) -> String {
        let tid: i64 = ticket_id.parse().unwrap_or(0);
        match crate::dashboard::upload_attachment(tid, &file_path) {
            Ok(()) => serde_json::json!({"success": true}).to_string(),
            Err(e) => serde_json::json!({"success": false, "error": e.to_string()}).to_string(),
        }
    }

    fn pick_file(&self) -> String {
        "".to_string()
    }

    fn check_invite_code(&self, code: String) -> String {
        match crate::dashboard::check_invite_code(&code) {
            Ok(()) => String::new(),
            Err(e) => {
                let msg = e.to_string();
                if msg.is_empty() { "Connection failed".to_owned() } else { msg }
            }
        }
    }

    fn take_dashboard_link_error(&self) -> String {
        crate::dashboard::take_link_error()
    }

    fn get_ticket_destination(&self) -> String {
        let linked = !crate::dashboard::get_dashboard_user_id().is_empty();
        let contact = if linked {
            crate::dashboard::fetch_support_contact().unwrap_or(serde_json::json!({}))
        } else {
            serde_json::json!({})
        };
        serde_json::json!({
            "linked": linked,
            "name": contact["name"].as_str().unwrap_or(""),
            "support_email": contact["support_email"].as_str().unwrap_or(""),
            "phone": contact["phone"].as_str().unwrap_or(""),
            "portal_url": crate::dashboard::portal_url(contact["portal_slug"].as_str().unwrap_or("")),
        }).to_string()
    }

    fn open_support_portal(&self, url: String) -> String {
        if url.starts_with("https://") {
            self.open_url(url);
        }
        "".to_string()
    }

    fn get_ticket_reply_counter(&self) -> String {
        crate::dashboard::get_ticket_reply_counter().to_string()
    }

    fn get_ticket_status_counter(&self) -> String {
        crate::dashboard::get_ticket_status_counter().to_string()
    }

    fn open_ticket_window(&self) -> String {
        match crate::run_me(vec!["--ticket"]) {
            Ok(_) => "ok".to_string(),
            Err(e) => e.to_string(),
        }
    }

    fn get_file_size(&self, path: String) -> String {
        let path = crate::dashboard::percent_decode_path(&path);
        match std::fs::metadata(&path) {
            Ok(m) => m.len().to_string(),
            Err(_) => "0".to_string(),
        }
    }
}

impl wireui::EventHandler for UI {
    wireui::dispatch_script_call! {
        fn t(String);
        //fn get_api_server();
        fn is_xfce();
        //fn using_public_server();
        fn get_id();
        fn get_local_ip();
        fn temporary_password();
        fn update_temporary_password();
        fn permanent_password();
        fn set_permanent_password(String);
        fn guest_password();
        fn set_guest_password(String);
        fn get_remote_id();
        fn set_remote_id(String);
        fn closing(i32, i32, i32, i32);
        fn get_size();
        fn new_remote(String, String, bool);
        fn send_wol(String);
        fn remove_peer(String);
        fn remove_discovered(String);
        fn get_connect_status();
        fn get_mouse_time();
        fn check_mouse_time();
        fn get_recent_sessions();
        fn get_peer(String);
        fn get_fav();
        fn store_fav(Value);
        fn recent_sessions_updated();
        fn get_icon();
        fn install_me(String, String);
        fn is_installed();
        fn is_remote_printing_available();
        fn is_drm_capture_supported();
        fn is_start_on_boot();
        fn set_start_on_boot(bool);
        fn is_root();
        fn is_release();
        fn set_socks(String, String, String, String);
        fn get_socks();
        fn is_share_rdp();
        fn set_share_rdp(bool);
        fn is_installed_lower_version();
        fn install_path();
        fn install_options();
        fn goto_install();
        fn is_process_trusted(bool);
        fn is_can_screen_recording(bool);
        fn is_installed_daemon(bool);
        fn get_error();
        fn is_login_wayland();
        fn current_is_wayland();
        fn get_options();
        fn get_option(String);
        fn get_local_option(String);
        fn set_local_option(String, String);
        fn get_peer_option(String, String);
        fn peer_has_password(String);
        fn forget_password(String);
        fn set_peer_option(String, String, String);
        //fn get_license();
        fn test_if_valid_server(String);
        fn get_sound_inputs();
        fn get_microphone_access();
        fn request_microphone_access();
        fn get_mac_permissions();
        fn request_mac_permission(String);
        fn set_options(Value);
        fn set_option(String, String);
        //fn get_software_update_url();
        fn get_new_version();
        fn get_version();
        fn get_fingerprint();
        //fn update_me(String);
        fn show_run_without_install();
        fn run_without_install();
        fn get_app_name();
        fn get_display_name();
        fn get_software_store_path();
        fn get_software_ext();
        fn open_url(String);
		fn run_temp_update();
        //fn change_id(String);
        fn get_async_job_status();
        fn post_request(String, String, String);
		fn get_request(String, String);
        //fn is_ok_change_id();
        fn create_shortcut(String);
        fn discover();
        fn get_lan_peers();
        fn get_uuid();
        fn has_hwcodec();
        fn has_vram();
        fn get_langs();
        fn video_save_directory(bool);
        fn recordings_directory();
        fn open_recordings_directory();
        fn handle_relay_id(String);
        fn get_login_device_info();
        fn support_remove_wallpaper();
        fn has_valid_2fa();
        fn generate2fa();
        fn generate_2fa_img_src(String);
        fn verify2fa(String);
        fn check_hwcodec();        
        fn requires_update();
        fn running_qs();
        fn is_incoming_only();
        fn is_option_fixed(String);
        fn is_close_protected();
        fn verify_uninstall_code(String);
        fn perform_uninstall();
        fn is_prompt_device_name();
        fn is_custom_build();
        fn custom_build_id();
        fn is_help_desk_hidden();
        fn get_hostname();
        fn get_dashboard_url();
		fn set_version_sync();
		fn copy_text(String);
        fn get_config_option(String);
        fn set_config_option(String, String);
        fn get_custom_api_url();
        fn set_custom_api_url(String);
        fn send_peer_invite(String, String, String);
        fn submit_ticket(String, String, String, String);
        fn get_my_tickets();
        fn get_conversation(String);
        fn get_attachments(String);
        fn add_reply(String, String);
        fn upload_attachment(String, String);
        fn pick_file();
        fn check_invite_code(String);
        fn take_dashboard_link_error();
        fn get_ticket_reply_counter();
        fn get_ticket_status_counter();
        fn get_ticket_destination();
        fn open_ticket_window();
        fn open_support_portal(String);
        fn get_file_size(String);
    }
}

impl wireui::host::HostHandler for UIHostHandler {
    fn on_graphics_critical_failure(&mut self) {
        log::error!("Critical rendering error: e.g. DirectX gfx driver error. Most probably bad gfx drivers.");
    }
}

use serde::Deserialize;
#[derive(Deserialize)]
struct Version {
    winversion: String,
    linuxversion: String,
    macversion: String,
    none: String,
}

fn platform_version(v: &serde_json::Value) -> Option<String> {
	let body = serde_json::from_value::<Version>(v.clone()).ok()?;
	Some(if cfg!(windows) {
		body.winversion
	} else if cfg!(target_os = "macos") {
		body.macversion
	} else if cfg!(target_os = "linux") {
		body.linuxversion
	} else {
		body.none
	})
}

async fn get_version_(refresh_api: bool) -> String {
	if refresh_api {
		hbb_common::api::erase_api().await;
	}
	let v = match hbb_common::api::call_api().await {
		Ok(v) => v,
		Err(e) => {
			log::error!("get_version error {:?}, refresh_api: {:?}", e, refresh_api);
			return "".to_owned();
		}
	};
	if let Some(version) = platform_version(&v) {
		return version;
	}
	if let Some(version) = hbb_common::api::call_public_api().await.ok().as_ref().and_then(platform_version) {
		return version;
	}
	let json_str = serde_json::to_string(&v).unwrap_or_default();
	let b64 = base64::encode(json_str, base64::Variant::Original);
	log::error!("Invalid API response: {}", b64);
	"".to_owned()
}

fn copy_text(text: &str) -> bool {
    let text_clip = Clipboard {
        compress: false,
        content: text.to_owned().into_bytes().into(),
        format: ClipboardFormat::Text.into(),
        ..Default::default()
    };
    set_clipboard_now(vec![text_clip], ClipboardSide::Client)
}

pub fn set_version_sync() -> bool {
    let rt = Runtime::new().unwrap();
    rt.block_on(async { store_api_version(get_version_(true).await) })
}

#[tokio::main]
pub async fn set_version() {
    store_api_version(get_version_(false).await);
}

fn store_api_version(version: String) -> bool {
    if version.is_empty() {
        return false;
    }
    Config::set_option("api_version".to_owned(), version);
    true
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn update_package_url() -> String {
    const DOWNLOAD_PAGE: &str = "https://www.hoptodesk.com/download";
    if cfg!(target_os = "freebsd") {
        return DOWNLOAD_PAGE.to_owned();
    }
    let running_appimage = std::env::var("APPDIR")
        .map_or(false, |dir| std::path::Path::new(&dir).join("AppRun").exists());
    if running_appimage {
        return if cfg!(target_arch = "x86_64") {
            "https://dl.hoptodesk.com/hoptodesk.AppImage".to_owned()
        } else {
            DOWNLOAD_PAGE.to_owned()
        };
    }
    let rpm_based = std::path::Path::new("/usr/bin/rpm").exists()
        && !std::path::Path::new("/usr/bin/dpkg").exists();
    if rpm_based {
        return if cfg!(target_arch = "x86_64") {
            "https://dl.hoptodesk.com/hoptodesk.rpm".to_owned()
        } else {
            DOWNLOAD_PAGE.to_owned()
        };
    }
    if cfg!(target_arch = "aarch64") {
        return "https://dl.hoptodesk.com/hoptodesk-arm64.deb".to_owned();
    }
    if cfg!(target_arch = "x86_64") {
        return "https://dl.hoptodesk.com/hoptodesk.deb".to_owned();
    }
    DOWNLOAD_PAGE.to_owned()
}

pub const SYSTEM_SOUND: &str = "System Sound";

#[cfg(target_os = "windows")]
fn has_system_sound() -> bool {
    true
}

#[cfg(target_os = "macos")]
fn has_system_sound() -> bool {
    #[cfg(feature = "screencapturekit")]
    return crate::server::audio_service::is_screen_capture_kit_available();
    #[cfg(not(feature = "screencapturekit"))]
    return false;
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn has_system_sound() -> bool {
    !crate::platform::linux::get_pa_monitor().is_empty()
}

#[cfg(not(target_os = "linux"))]
fn get_sound_inputs() -> Vec<String> {
    let mut out = Vec::new();
    if has_system_sound() {
        out.push(SYSTEM_SOUND.to_owned());
    }
    #[cfg(target_os = "macos")]
    if !crate::platform::macos::is_microphone_authorized() {
        return out;
    }
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    if let Ok(devices) = host.devices() {
        for device in devices {
            if device.default_input_config().is_err() {
                continue;
            }
            if let Ok(name) = device.name() {
                out.push(name);
            }
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn get_sound_inputs() -> Vec<String> {
    let mut out = Vec::new();
    if has_system_sound() {
        out.push(SYSTEM_SOUND.to_owned());
    }
    out.extend(
        crate::platform::linux::get_pa_sources()
            .drain(..)
            .filter(|x| !x.0.contains("monitor"))
            .map(|x| x.1),
    );
    out
}

// sacrifice some memory
pub fn value_crash_workaround(values: &[Value]) -> Arc<Vec<Value>> {
    let persist = Arc::new(values.to_vec());
    STUPID_VALUES.lock().unwrap().push(persist.clone());
    persist
}

#[inline]
pub fn new_remote(id: String, remote_type: String, force_relay: bool, self_id_opt: Option<String>, invite_password_opt: Option<String>) {
    let mut lock = CHILDREN.lock().unwrap();
    let mut args = vec![];
    let mut secrets: Vec<(&str, String)> = Vec::new();
    if remote_type == "invite" {
        args.push("--invite".to_string());
        args.push(id.clone());
        if let Some(sid) = self_id_opt {
            args.push(sid);
        }
        if let Some(pwd) = invite_password_opt {
            secrets.push((crate::common::ENV_INVITE_PASSWORD, pwd));
        }
    } else {
        args.push(format!("--{}", remote_type));
        args.push(id.clone());
        if let Some(pwd) = invite_password_opt {
            secrets.push((crate::common::ENV_PEER_PASSWORD, pwd));
        }
    }

    if force_relay {
        if remote_type != "invite" {
            args.push("".to_string());
        }
        args.push("--relay".to_string());
    }
    let key = (id.clone(), remote_type.clone());
    if let Some(c) = lock.1.get_mut(&key) {
        if let Ok(Some(_)) = c.try_wait() {
            lock.1.remove(&key);
        } else {
            if remote_type == "rdp" {
                allow_err!(c.kill());
                std::thread::sleep(std::time::Duration::from_millis(30));
                c.try_wait().ok();
                lock.1.remove(&key);
            } else {
                #[cfg(windows)]
                unsafe { winapi::um::winuser::AllowSetForegroundWindow(c.id()) };
                if let Some(stdin) = CHILD_STDINS.lock().unwrap().get_mut(&key) {
                    write_child_line(&id, stdin, "focus");
                }
                return;
            }
        }
    }

    // Spawn with piped stdin so MCP can send commands (e.g. dismiss_dialog)
    let cmd = {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            if let Ok(appdir) = std::env::var("APPDIR") {
                let appimage_cmd = std::path::Path::new(&appdir).join("AppRun");
                if appimage_cmd.exists() { appimage_cmd } else { match std::env::current_exe() { Ok(c) => c, Err(e) => { log::error!("Failed to get exe: {}", e); return; } } }
            } else {
                match std::env::current_exe() { Ok(c) => c, Err(e) => { log::error!("Failed to get exe: {}", e); return; } }
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        match std::env::current_exe() { Ok(c) => c, Err(e) => { log::error!("Failed to get exe: {}", e); return; } }
    };
    match std::process::Command::new(cmd)
        .args(&args)
        .envs(secrets.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            let stdin = child.stdin.take();
            if let Some(s) = stdin {
                CHILD_STDINS.lock().unwrap().insert(key.clone(), s);
            }
            lock.1.insert(key, child);
        }
        Err(err) => {
            log::error!("Failed to spawn remote: {}", err);
        }
    }
}

#[inline]
pub fn recent_sessions_updated() -> bool {
    let mut children = CHILDREN.lock().unwrap();
    if children.0 {
        children.0 = false;
        true
    } else {
        false
    }
}

/// Kill child process for a specific peer connection (used by MCP server)
pub fn close_remote_connection(id: &str) {
    CHILD_STDINS.lock().unwrap().retain(|k, _| k.0 != id);
    CHILDREN.lock().unwrap().1.retain(|k, child| {
        if k.0 == id {
            match child.kill() {
                Ok(_) => return false,
                Err(e) => log::error!("Failed to kill remote {id}: {e}")
            }
        }
        true
    });
}

/// Send a command to a child connection process via its stdin pipe (used by MCP server)
pub fn send_to_child(peer_id: &str, msg: &str) -> bool {
    let mut stdins = CHILD_STDINS.lock().unwrap();
    let key = stdins
        .keys()
        .filter(|k| k.0 == peer_id)
        .max_by_key(|k| k.1 == "connect")
        .cloned();
    match key.and_then(|k| stdins.get_mut(&k)) {
        Some(stdin) => write_child_line(peer_id, stdin, msg),
        None => false,
    }
}

fn write_child_line(peer_id: &str, stdin: &mut std::process::ChildStdin, msg: &str) -> bool {
    if let Err(e) = writeln!(stdin, "{}", msg) {
        log::error!("Failed to send to child {}: {}", peer_id, e);
        return false;
    }
    let _ = stdin.flush();
    true
}

/// Send dismiss command to all active child connections (used by MCP server)
pub fn send_dismiss_to_all_children() -> usize {
    let mut stdins = CHILD_STDINS.lock().unwrap();
    stdins
        .iter_mut()
        .map(|(k, stdin)| write_child_line(&k.0, stdin, "dismiss"))
        .filter(|sent| *sent)
        .count()
}

/// List active connections with their alive status (used by MCP server)
pub fn list_active_connections() -> Vec<(String, String, bool)> {
    let mut lock = CHILDREN.lock().unwrap();
    lock.1.iter_mut().map(|((id, conn_type), child)| {
        let alive = child.try_wait().map(|s| s.is_none()).unwrap_or(false);
        (id.clone(), conn_type.clone(), alive)
    }).collect()
}

pub fn get_icon() -> String {
	#[cfg(target_os = "macos")]
    {
        let icon_data = include_bytes!("../res/128x128.png");
        let base64_str = base64::encode(icon_data, base64::Variant::Original);
        format!("data:image/png;base64,{}", base64_str)
    }
	#[cfg(not(target_os = "macos"))]
    {
        let icon_data = include_bytes!("../res/icon.ico");
        let base64_str = base64::encode(icon_data, base64::Variant::Original);
        format!("data:image/x-icon;base64,{}", base64_str)
    }
}