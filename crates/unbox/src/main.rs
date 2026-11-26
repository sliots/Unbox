//! Unbox — double-click to extract.
//!
//! One executable plays three roles:
//! * `unbox [--here] [archive]` — the progress window (UI process); later ones hand
//!   their archive to the first window, which extracts them one after another;
//! * `unbox --worker …` — the isolated extraction worker driven over stdin/stdout,
//!   which hosts 7z.dll so a crash in a decoder can never take the UI down;
//! * `unbox extract <archive>` — the same pipeline without a window;
//! * `unbox register | unregister | status` — per-user file associations.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod assoc;
#[cfg(windows)]
mod win;
#[cfg(windows)]
mod worker;

#[cfg(windows)]
fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let first = args.first().and_then(|a| a.to_str()).unwrap_or("");
    let code = match first {
        "--worker" => worker::run(&args[1..]),
        "extract" => {
            win::attach_parent_console();
            app::extract_cli(&args[1..])
        }
        "register" | "unregister" | "status" | "--help" | "-h" | "help" | "--version" | "-V" => {
            win::attach_parent_console();
            assoc::cli(first, &args[1..])
        }
        _ => app::run(&args),
    };
    std::process::exit(code);
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Unbox is a Windows application.");
    std::process::exit(1);
}
