//! Per-user file associations (`unbox register | unregister | status`).
//!
//! Unbox registers itself as an *Open with* candidate (ProgIDs + Capabilities) for
//! every associated catalog format. It never overrides the user's choice: Windows
//! asks, or the user picks Unbox in Settings → Default apps.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use unbox_core::catalog::{associated, ArchiveFormat};
use winreg::enums::*;
use winreg::{RegKey, RegValue};

include!(concat!(env!("OUT_DIR"), "/icon_ids.rs"));

const APP_ID: &str = "Unbox";
const CAPABILITIES: &str = r"Software\Unbox\Capabilities";

fn icon_id(key: &str) -> u16 {
    FORMAT_ICON_IDS
        .iter()
        .find(|(k, _)| *k == key)
        .or_else(|| FORMAT_ICON_IDS.iter().find(|(k, _)| *k == "origin"))
        .map(|(_, id)| *id)
        .unwrap_or(1)
}

/// The executable path to register. A Scoop install lives in a versioned directory
/// next to a `current` junction; registering the junction keeps associations valid
/// across `scoop update`.
pub fn stable_exe() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    let exe = std::fs::canonicalize(&exe).map(|p| PathBuf::from(crate::win::fs_strip(&p))).unwrap_or(exe);
    if let (Some(version_dir), Some(name)) = (exe.parent(), exe.file_name()) {
        if let Some(app_dir) = version_dir.parent() {
            let is_scoop = app_dir.parent().and_then(|p| p.file_name()).is_some_and(|n| n.eq_ignore_ascii_case("apps"));
            let current = app_dir.join("current").join(name);
            if is_scoop && version_dir.file_name().is_some_and(|n| n != "current") && current.is_file() {
                return current;
            }
        }
    }
    exe
}

fn open_command(exe: &Path) -> String {
    format!("\"{}\" \"%1\"", exe.display())
}

fn prog_id(format: &ArchiveFormat) -> String {
    format!("{APP_ID}{}", format.suffix)
}

pub fn register(exe: &Path) -> std::io::Result<usize> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (classes, _) = hkcu.create_subkey(r"Software\Classes")?;
    let (caps, _) = hkcu.create_subkey(CAPABILITIES)?;
    let (assoc, _) = caps.create_subkey("FileAssociations")?;
    let command = open_command(exe);
    let exe_text = exe.display().to_string();

    // Windows may remember an "Open with" choice by executable name.
    let (app_cmd, _) = classes.create_subkey(r"Applications\unbox.exe\shell\open\command")?;
    app_cmd.set_value("", &command)?;
    let (app_key, _) = classes.create_subkey(r"Applications\unbox.exe")?;
    app_key.set_value("FriendlyAppName", &"Unbox")?;

    caps.set_value("ApplicationName", &"Unbox")?;
    caps.set_value("ApplicationDescription", &"双击即解压")?;
    caps.set_value("ApplicationIcon", &format!("\"{exe_text}\",0"))?;
    let (registered, _) = hkcu.create_subkey(r"Software\RegisteredApplications")?;
    registered.set_value(APP_ID, &CAPABILITIES)?;

    let mut count = 0;
    for format in associated() {
        let id = prog_id(format);
        let (prog, _) = classes.create_subkey(&id)?;
        prog.set_value("", &format.display_name)?;
        prog.set_value("FriendlyTypeName", &format.display_name)?;
        let (icon, _) = prog.create_subkey("DefaultIcon")?;
        icon.set_value("", &format!("\"{exe_text}\",-{}", icon_id(&format.icon_key)))?;
        let (open, _) = prog.create_subkey(r"shell\open")?;
        open.set_value("FriendlyAppName", &"Unbox")?;
        let (cmd, _) = prog.create_subkey(r"shell\open\command")?;
        cmd.set_value("", &command)?;

        let (open_with, _) = classes.create_subkey(format!(r"{}\OpenWithProgids", format.suffix))?;
        open_with.set_raw_value(&id, &RegValue { vtype: REG_NONE, bytes: Vec::new().into() })?;
        assoc.set_value(&format.suffix, &id)?;
        count += 1;
    }
    notify_shell();
    Ok(count)
}

fn command_is_ours(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    ["\\unbox.exe\" \"%1\"", "\\unzipproxy.exe\" \"%1\""].iter().any(|s| lower.ends_with(s)) && lower.starts_with('"')
}

pub fn unregister() -> Vec<String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let mut failures = Vec::new();
    let Ok(classes) = hkcu.open_subkey_with_flags(r"Software\Classes", KEY_ALL_ACCESS) else { return failures };
    let registered = hkcu.open_subkey_with_flags(r"Software\RegisteredApplications", KEY_ALL_ACCESS).ok();
    let mut known: Vec<String> = associated().map(|f| f.suffix.clone()).collect();

    for id in [APP_ID, "UnzipProxy"] {
        let cap_path = format!(r"Software\{id}\Capabilities");
        let cap = hkcu.open_subkey_with_flags(&cap_path, KEY_ALL_ACCESS).ok();
        let assoc = cap.as_ref().and_then(|c| c.open_subkey_with_flags("FileAssociations", KEY_ALL_ACCESS).ok());
        if let Some(assoc) = &assoc {
            known.extend(assoc.enum_values().filter_map(|v| v.ok()).map(|(name, _)| name));
        }
        known.sort();
        known.dedup();
        for ext in &known {
            if !ext.starts_with('.') || ext.len() < 2 || !ext[1..].bytes().all(|b| b.is_ascii_alphanumeric()) {
                continue;
            }
            let pid = format!("{id}{ext}");
            let owned_mapping = assoc.as_ref().and_then(|a| a.get_value::<String, _>(ext).ok()).as_deref() == Some(pid.as_str());
            let owned_prog = classes
                .open_subkey(format!(r"{pid}\shell\open\command"))
                .and_then(|k| k.get_value::<String, _>(""))
                .is_ok_and(|c| command_is_ours(&c));
            if let Ok(open_with) = classes.open_subkey_with_flags(format!(r"{ext}\OpenWithProgids"), KEY_ALL_ACCESS) {
                if open_with.get_raw_value(&pid).is_ok() {
                    if let Err(e) = open_with.delete_value(&pid) {
                        failures.push(format!("{pid} - {e}"));
                    }
                }
            }
            if owned_mapping || owned_prog {
                if let Err(e) = classes.delete_subkey_all(&pid) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        failures.push(format!("{pid} - {e}"));
                    }
                }
            }
            if owned_mapping {
                let _ = assoc.as_ref().unwrap().delete_value(ext);
            }
        }
        if let Some(registered) = &registered {
            if registered.get_value::<String, _>(id).ok().as_deref() == Some(cap_path.as_str()) {
                let _ = registered.delete_value(id);
            }
        }
        if cap.is_some() {
            let _ = hkcu.delete_subkey_all(format!(r"Software\{id}"));
        }
    }
    for exe in ["unbox.exe", "Unbox.exe", "UnzipProxy.exe"] {
        let path = format!(r"Applications\{exe}");
        let ours = classes
            .open_subkey(format!(r"{path}\shell\open\command"))
            .and_then(|k| k.get_value::<String, _>(""))
            .is_ok_and(|c| command_is_ours(&c));
        if ours {
            let _ = classes.delete_subkey_all(&path);
        }
    }
    notify_shell();
    failures
}

pub fn status(exe: &Path) -> String {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let caps = hkcu.open_subkey(format!(r"{CAPABILITIES}\FileAssociations")).ok();
    let registered = hkcu.open_subkey(r"Software\RegisteredApplications").and_then(|k| k.get_value::<String, _>(APP_ID)).ok();
    if caps.is_none() && registered.is_none() {
        return "未登记".into();
    }
    let command = open_command(exe);
    let total = associated().count();
    let ok = associated().all(|format| {
        let id = prog_id(format);
        caps.as_ref().and_then(|c| c.get_value::<String, _>(&format.suffix).ok()).as_deref() == Some(id.as_str())
            && hkcu
                .open_subkey(format!(r"Software\Classes\{id}\shell\open\command"))
                .and_then(|k| k.get_value::<String, _>(""))
                .is_ok_and(|c| c == command)
            && hkcu.open_subkey(format!(r"Software\Classes\{}\OpenWithProgids", format.suffix)).is_ok_and(|k| k.get_raw_value(&id).is_ok())
    });
    if ok && registered.as_deref() == Some(CAPABILITIES) {
        format!("完整登记（{total} 项）")
    } else {
        "部分登记或指向旧目录，请运行 unbox register 修复".into()
    }
}

fn notify_shell() {
    use windows_sys::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
}

pub fn open_default_apps_settings() {
    let _ = std::process::Command::new("explorer.exe").arg(format!("ms-settings:defaultapps?registeredAppUser={APP_ID}")).spawn();
}

const HELP: &str = "Unbox — 双击即解压

用法:
  unbox [压缩包]           打开进度窗口并解压（无参数时弹出文件选择框）
  unbox register [--no-settings]
                           为当前用户登记文件关联，并打开“默认应用”设置
  unbox unregister         移除当前用户的 Unbox 文件关联
  unbox status             查看关联状态
  unbox extract <压缩包> [--password <密码>] [--skip-invalid] [--cp <代码页>] [--recycle]
                           无窗口解压，成功时输出目标目录（退出码 0 成功 / 2 部分完成 / 1 失败）
  unbox --version          显示版本";

pub fn cli(command: &str, args: &[OsString]) -> i32 {
    let exe = stable_exe();
    match command {
        "register" => match register(&exe) {
            Ok(count) => {
                println!("[OK] 已为当前用户登记 {count} 个文件关联 → {}", exe.display());
                if !args.iter().any(|a| a == "--no-settings") {
                    println!("请在“默认应用”中把需要双击解压的扩展名设为由 Unbox 打开。");
                    open_default_apps_settings();
                }
                0
            }
            Err(e) => {
                eprintln!("登记失败：{e}");
                1
            }
        },
        "unregister" => {
            let failures = unregister();
            if failures.is_empty() {
                println!("[OK] 已移除当前用户的 Unbox 文件关联。");
                0
            } else {
                for failure in failures {
                    eprintln!("{failure}");
                }
                1
            }
        }
        "status" => {
            println!("[关联状态] {}", status(&exe));
            0
        }
        "--version" | "-V" => {
            println!("unbox {}", env!("CARGO_PKG_VERSION"));
            0
        }
        _ => {
            println!("{HELP}");
            0
        }
    }
}
