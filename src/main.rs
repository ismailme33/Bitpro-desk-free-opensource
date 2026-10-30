#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use libhoptodesk::*;
#[cfg(windows)]
use std::ptr;
#[cfg(feature = "standalone")]
use {
    winapi::um::winreg::{RegCreateKeyExA, RegSetValueExA, HKEY_CURRENT_USER},
	winapi::shared::minwindef::HKEY,
	winapi::um::winnt::REG_SZ
};
#[cfg(not(any(target_os = "android", target_os = "ios", feature = "flutter")))]
use std::{env, fs};

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use hbb_common::{config::{Config},};

#[cfg(windows)]
use nt_version;

#[cfg(windows)]
use winapi::{
    um::{
        winbase::CreateFileMappingA,
        memoryapi::MapViewOfFile,
        handleapi::INVALID_HANDLE_VALUE,
        winnt::{PAGE_READWRITE},
        errhandlingapi::GetLastError,
    },
};
#[cfg(windows)]
const FILE_MAP_ALL_ACCESS: u32 = 0xF001F;

#[cfg(windows)]
static PNG_DATA: &[u8] = include_bytes!("../res/PrivacyMode.png");

#[cfg(any(target_os = "android", target_os = "ios", feature = "flutter"))]
fn main() {
    if !common::global_init() {
        return;
    }
    common::test_rendezvous_server();
    /*common::test_nat_type();
    #[cfg(target_os = "android")]
    crate::common::check_software_update();*/
    common::global_clean();
}

#[cfg(target_os = "macos")]
fn find_macos_invite_code() -> Option<String> {
	if let Some(code) = find_invite_dmg_via_spotlight() {
		return Some(code);
	}
	if let Some(home) = std::env::var_os("HOME") {
		let home = std::path::Path::new(&home);
		for dir_name in &["Downloads", "Desktop", "Documents"] {
			if let Some(code) = search_dir_for_invite_dmg(&home.join(dir_name)) {
				return Some(code);
			}
		}
	}
	if let Some(code) = search_dir_for_invite_dmg(std::path::Path::new("/tmp")) {
		return Some(code);
	}
	None
}

#[cfg(target_os = "macos")]
fn find_invite_dmg_via_spotlight() -> Option<String> {
	let mut candidates: Vec<(std::time::SystemTime, String)> = Vec::new();
	for prefix in invite_file_prefixes() {
		let Ok(output) = std::process::Command::new("mdfind")
			.arg(format!("kMDItemFSName == '{}*.dmg'c", prefix))
			.output()
		else {
			continue;
		};
		if !output.status.success() {
			continue;
		}
		let stdout = String::from_utf8_lossy(&output.stdout);
		for line in stdout.lines() {
			let path = std::path::Path::new(line.trim());
			let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
				continue;
			};
			let Some(code) = invite_code_from_package_name(&name, &[".dmg"]) else {
				continue;
			};
			if let Ok(meta) = std::fs::metadata(path) {
				if let Ok(modified) = meta.modified() {
					candidates.push((modified, code));
				}
			}
		}
	}
	candidates.sort_by(|a, b| b.0.cmp(&a.0));
	candidates.into_iter().next().map(|(_, code)| code)
}

#[cfg(target_os = "macos")]
fn search_dir_for_invite_dmg(dir: &std::path::Path) -> Option<String> {
	let entries = std::fs::read_dir(dir).ok()?;
	let mut candidates: Vec<(std::time::SystemTime, String)> = Vec::new();
	for entry in entries.flatten() {
		let name = entry.file_name().to_string_lossy().to_string();
		if let Some(code) = invite_code_from_package_name(&name, &[".dmg"]) {
			if let Ok(meta) = entry.metadata() {
				if let Ok(modified) = meta.modified() {
					candidates.push((modified, code));
				}
			}
		}
	}
	candidates.sort_by(|a, b| b.0.cmp(&a.0));
	candidates.into_iter().next().map(|(_, code)| code)
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
fn invite_file_prefixes() -> Vec<String> {
	let mut prefixes = vec!["hoptodeskpro-".to_owned()];
	let branded: String = common::get_app_name()
		.to_lowercase()
		.chars()
		.filter(|c| c.is_ascii_alphanumeric())
		.collect();
	if !branded.is_empty() && branded != "hoptodeskpro" {
		prefixes.push(format!("{}-", branded));
	}
	prefixes
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
fn invite_code_from_package_name(name: &str, suffixes: &[&str]) -> Option<String> {
	if !name.is_ascii() {
		return None;
	}
	let lower = name.to_lowercase();
	let suffix = suffixes.iter().find(|s| lower.ends_with(*s))?;
	let prefix_len = invite_file_prefixes()
		.iter()
		.find(|p| lower.starts_with(p.as_str()))
		.map(|p| p.len())?;
	validate_invite_code(&name[prefix_len..name.len() - suffix.len()])
}

#[cfg(not(any(target_os = "android", target_os = "ios", feature = "flutter")))]
fn invite_code_from_exe_name(exe_file_name: &str) -> Option<String> {
	let stem = exe_file_name
		.rsplit_once('.')
		.map(|(head, _)| head)
		.unwrap_or(exe_file_name);
	stem.split('-').rev().find_map(validate_invite_code)
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn validate_invite_code(s: &str) -> Option<String> {
	if s.len() != 16 {
		return None;
	}
	let mut has_uppercase = false;
	for c in s.chars() {
		if c.is_ascii_uppercase() {
			has_uppercase = true;
		} else if !(c.is_ascii_lowercase() || c.is_ascii_digit()) {
			return None;
		}
	}
	if has_uppercase { Some(s.to_string()) } else { None }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn find_linux_invite_code() -> Option<String> {
	// Search current user's home dirs first
	if let Some(home) = std::env::var_os("HOME") {
		let home = std::path::Path::new(&home);
		for dir_name in &["Downloads", "Desktop", "Documents"] {
			if let Some(code) = search_dir_for_invite_deb(&home.join(dir_name)) {
				return Some(code);
			}
		}
		if let Some(code) = search_dir_for_invite_deb(home) {
			return Some(code);
		}
	}
	// Search /tmp
	if let Some(code) = search_dir_for_invite_deb(std::path::Path::new("/tmp")) {
		return Some(code);
	}
	// When running as root (systemd service), also search all users' Downloads
	// since the .deb was likely downloaded by a regular user
	if let Ok(entries) = std::fs::read_dir("/home") {
		for entry in entries.flatten() {
			let user_home = entry.path();
			if !user_home.is_dir() {
				continue;
			}
			for dir_name in &["Downloads", "Desktop", "Documents"] {
				if let Some(code) = search_dir_for_invite_deb(&user_home.join(dir_name)) {
					return Some(code);
				}
			}
		}
	}
	// Also check /root/Downloads if HOME isn't /root
	let root_home = std::path::Path::new("/root");
	if std::env::var_os("HOME").map_or(true, |h| std::path::Path::new(&h) != root_home) {
		for dir_name in &["Downloads", "Desktop", "Documents"] {
			if let Some(code) = search_dir_for_invite_deb(&root_home.join(dir_name)) {
				return Some(code);
			}
		}
	}
	None
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn search_dir_for_invite_deb(dir: &std::path::Path) -> Option<String> {
	let entries = std::fs::read_dir(dir).ok()?;
	let mut candidates: Vec<(std::time::SystemTime, String)> = Vec::new();
	// Every package format an invite can hand out on these platforms.
	let suffixes = [".deb", ".rpm", ".appimage", ".pkg", ".tar.zst"];
	for entry in entries.flatten() {
		let name = entry.file_name().to_string_lossy().to_string();
		if let Some(code) = invite_code_from_package_name(&name, &suffixes) {
			if let Ok(meta) = entry.metadata() {
				if let Ok(modified) = meta.modified() {
					candidates.push((modified, code));
				}
			}
		}
	}
	// Use the most recently modified package
	candidates.sort_by(|a, b| b.0.cmp(&a.0));
	candidates.into_iter().next().map(|(_, code)| code)
}

#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    feature = "cli",
    feature = "flutter"
)))]
fn main() {
    #[cfg(feature = "standalone")]
	if !crate::platform::is_installed() {
		let rule_name = "HopToDesk";
		let exe_path = env::current_exe().expect("Failed to get current executable path");
		let cu_key: HKEY = HKEY_CURRENT_USER as HKEY;
	
		let software_classes_key = format!("Software\\Classes\\{}", rule_name);
		set_registry_string_value(&cu_key, &software_classes_key, "URL Protocol", "").unwrap();
		set_registry_string_value(&cu_key, &software_classes_key, "", "URL:hoptodesk Protocol").unwrap();
		set_registry_string_value(
			&cu_key,
			&format!("{}\\shell\\open\\command", software_classes_key),
			"",
			&format!(r#""{}" "--connect" "%1""#, exe_path.to_str().expect("Failed to convert executable path to string")),
		).unwrap();

		
	
	}	

	#[cfg(not(any(target_os = "android", target_os = "ios", feature = "flutter")))]
    {
		let exe_path = env::current_exe().expect("Failed to get current executable file name");
		let exe_file_name = exe_path
			.file_name()
			.expect("Failed to extract file name")
			.to_string_lossy()
			.to_string();

			let is_main_launch = std::env::args().nth(1).map_or(true, |a| !a.starts_with("--"));
			if is_main_launch {
				if let Some(invite_code) = invite_code_from_exe_name(&exe_file_name) {
					if Config::get_option("last_enrolled_invite_code") != invite_code {
						hbb_common::config::Config::set_option("invite_code".to_owned(), invite_code.clone());
						let primary = Config::path("InviteCode.toml");
						if let Some(parent) = primary.parent() {
							fs::create_dir_all(parent).ok();
						}
						fs::write(&primary, &invite_code).ok();
						let shared = Config::shared_path("InviteCode.toml");
						if let Some(parent) = shared.parent() {
							fs::create_dir_all(parent).ok();
						}
						fs::write(&shared, &invite_code).ok();
						Config::set_option("last_enrolled_invite_code".to_owned(), invite_code);
					}
				}
			}

		#[cfg(target_os = "macos")]
		{
			if hbb_common::config::Config::get_option("invite_code").is_empty()
				&& !std::path::Path::new(&hbb_common::config::Config::path("InviteCode.toml")).exists()
			{
				if let Some(code) = find_macos_invite_code() {
					let last = hbb_common::config::Config::get_option("last_enrolled_invite_code");
					if last != code {
						// Write to InviteCode.toml (survives config sync from root service)
						let invite_path = hbb_common::config::Config::path("InviteCode.toml");
						if let Some(parent) = invite_path.parent() {
							fs::create_dir_all(parent).ok();
						}
						fs::write(&invite_path, &code).ok();
						// Also set in-memory for immediate use
						hbb_common::config::Config::set_option("invite_code".to_owned(), code);
					}
				}
			}
		}

		// Linux: installed binary at /usr/bin/hoptodesk never has invite code in its name,
		#[cfg(any(target_os = "linux", target_os = "freebsd"))]
		{
			if hbb_common::config::Config::get_option("invite_code").is_empty()
				&& !std::path::Path::new(&hbb_common::config::Config::path("InviteCode.toml")).exists()
			{
				if let Some(code) = find_linux_invite_code() {
					let last = hbb_common::config::Config::get_option("last_enrolled_invite_code");
					if last != code {
						let invite_path = hbb_common::config::Config::path("InviteCode.toml");
						if let Some(parent) = invite_path.parent() {
							fs::create_dir_all(parent).ok();
						}
						fs::write(&invite_path, &code).ok();
						hbb_common::config::Config::set_option("invite_code".to_owned(), code);
					}
				}
			}
		}

	}

	#[cfg(windows)]
    unsafe {
        let size = PNG_DATA.len();
        let handle = CreateFileMappingA(
            INVALID_HANDLE_VALUE,
            ptr::null_mut(),  // NULL security attributes for local sharing
            PAGE_READWRITE,
            0,
            size as u32,
            b"Local\\PrivacyModeImage\0".as_ptr() as *const i8,
        );
        
        if handle.is_null() {
            eprintln!("Failed to create file mapping, error: {}. May already be mapped by another instance.", GetLastError());
        } else {
            let ptr = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size);
            if ptr.is_null() {
                eprintln!("Failed to map view, error: {}", GetLastError());
            } else {
                ptr::copy_nonoverlapping(PNG_DATA.as_ptr(), ptr as *mut u8, size);
            }
        }
    }

    #[cfg(target_os = "linux")]
    if std::env::var_os("GDK_BACKEND").is_none() {
        std::env::set_var("GDK_BACKEND", "x11,wayland");
    }

    if !common::global_init() {
        return;
    }
    #[cfg(all(windows, not(feature = "inline")))]
    {
		let is_windows_7: bool;
		match nt_version::get() {
			(6, 1, _) => is_windows_7 = true,
			_ => is_windows_7 = false,
		}
	   if is_windows_7 {
			//println!("Windows 7 detected.");
		} else {
			let shellscalingapi = unsafe {
				match winapi::um::libloaderapi::LoadLibraryA("api-ms-win-shcore-scaling-l1-1-0.dll\0".as_ptr() as *const i8) {
					hmodule if !hmodule.is_null() => {
						let address = winapi::um::libloaderapi::GetProcAddress(hmodule, "SetProcessDpiAwareness\0".as_ptr() as *const i8);
						if !address.is_null() {
							Some(std::mem::transmute::<_, unsafe extern "system" fn(u32)>(address))
						} else {
							None
						}
					}
					_ => None,
				}
			};

			if let Some(set_process_dpi_awareness) = shellscalingapi {
				unsafe {
					set_process_dpi_awareness(2);
				}
			}
		
		}
    }
    if let Some(args) = crate::core_main::core_main().as_mut() {
        ui::start(args);
    } else {
        // core_main returned None — check if this was a simple CLI command
        // (--get-id, --version) that should exit without further cleanup.
        let args: Vec<String> = env::args().collect();
        if args.iter().any(|a| a == "--get-id" || a == "--version") {
            return;
        }
    }
	common::global_clean();
}

#[cfg(feature = "cli")]
fn main() {
    if !common::global_init() {
        return;
    }
    use clap::App;
    use hbb_common::log;
    let args = format!(
        "-p, --port-forward=[PORT-FORWARD-OPTIONS] 'Format: remote-id:local-port:remote-port[:remote-host]'
        -c, --connect=[REMOTE_ID] 'test only'
        -k, --key=[KEY] ''
       -s, --server=[] 'Start server'",
    );
    let matches = App::new("hoptodesk")
        .version(crate::VERSION)
        .author("HopToDesk<info@hoptodesk.com>")
        .about("HopToDesk command line tool")
        .args_from_usage(&args)
        .get_matches();
    use hbb_common::{config::LocalConfig, env_logger::*};
    init_from_env(Env::default().filter_or(DEFAULT_FILTER_ENV, "info"));
    if let Some(p) = matches.value_of("port-forward") {
        let options: Vec<String> = p.split(":").map(|x| x.to_owned()).collect();
        if options.len() < 3 {
            log::error!("Wrong port-forward options");
            return;
        }
        let mut port = 0;
        if let Ok(v) = options[1].parse::<i32>() {
            port = v;
        } else {
            log::error!("Wrong local-port");
            return;
        }
        let mut remote_port = 0;
        if let Ok(v) = options[2].parse::<i32>() {
            remote_port = v;
        } else {
            log::error!("Wrong remote-port");
            return;
        }
        let mut remote_host = "localhost".to_owned();
        if options.len() > 3 {
            remote_host = options[3].clone();
        }
    } else if let Some(p) = matches.value_of("server") {
        log::info!("id={}", hbb_common::config::Config::get_id());
        crate::start_server(true);
    }
    common::global_clean();
}

#[cfg(feature = "standalone")]
fn set_registry_string_value(root_key: &HKEY, key_path: &str, value_name: &str, value: &str) -> Result<(), String> {
    unsafe {
        let key_path = std::ffi::CString::new(key_path).unwrap();
        let value_name = std::ffi::CString::new(value_name).unwrap();
        let value = std::ffi::CString::new(value).unwrap();

        let mut hkey: HKEY = ptr::null_mut();
        let mut disposition = 0;
        if RegCreateKeyExA(*root_key,key_path.as_ptr(),0,ptr::null_mut(),0,winapi::um::winnt::KEY_WRITE,ptr::null_mut(),&mut hkey,&mut disposition,) != 0
        {
            return Err("Error creating or opening registry key".to_string());
        }

        if RegSetValueExA(
            hkey,
            value_name.as_ptr(),
            0,
            REG_SZ,
            value.as_ptr() as *const _,
            (value.as_bytes().len() + 1) as u32,
        ) != 0
        {
            return Err("Error setting registry value".to_string());
        }
    }

    Ok(())
}

