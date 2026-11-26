//! Native window polish: DWM corners and frame, taskbar progress, attention
//! flashing and the completion sound.

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::HWND as WinHwnd;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::{
    ITaskbarList3, TaskbarList, TBPFLAG, TBPF_ERROR, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_PAUSED,
};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

static FINISHED_WAV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/finished.wav"));

pub fn hwnd_of(window: &slint::Window) -> Option<HWND> {
    match window.window_handle().window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as HWND),
        _ => None,
    }
}

/// Windows 11 (build 22000+) draws rounded frames; Windows 10 keeps square corners.
pub fn is_windows_11() -> bool {
    winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
        .and_then(|k| k.get_value::<String, _>("CurrentBuildNumber"))
        .ok()
        .and_then(|b| b.trim().parse::<u32>().ok())
        .is_some_and(|b| b >= 22000)
}

/// The window draws its own title bar and 1 px frame (like the 1.x WPF window):
/// ask DWM for rounded corners, drop its own border so the frame is not doubled, and
/// remove maximize/resize.
pub fn polish_window(hwnd: HWND) {
    unsafe {
        let corner: i32 = 2; // DWMWCP_ROUND
        DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE as u32, &corner as *const _ as *const _, 4);
        let no_border: u32 = 0xFFFF_FFFE; // DWMWA_COLOR_NONE
        DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR as u32, &no_border as *const _ as *const _, 4);
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let trimmed = style & !((WS_MAXIMIZEBOX | WS_THICKFRAME) as isize);
        if trimmed != style {
            SetWindowLongPtrW(hwnd, GWL_STYLE, trimmed);
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }
}

pub fn minimize(hwnd: HWND) {
    unsafe {
        ShowWindow(hwnd, SW_MINIMIZE);
    }
}

/// Bring the window forward and flash it until the user looks at it.
pub fn demand_attention(hwnd: HWND) {
    unsafe {
        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        SetForegroundWindow(hwnd);
        let info = FLASHWINFO {
            cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
            hwnd,
            dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
            uCount: u32::MAX,
            dwTimeout: 0,
        };
        FlashWindowEx(&info);
    }
}

/// Play the completion chime synchronously (call from a background thread).
pub fn play_finished_sound() {
    use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_MEMORY, SND_NODEFAULT, SND_SYNC};
    unsafe {
        PlaySoundW(FINISHED_WAV.as_ptr() as *const u16, std::ptr::null_mut(), SND_MEMORY | SND_SYNC | SND_NODEFAULT);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TaskbarState {
    None,
    Indeterminate,
    Normal,
    Paused,
    Error,
}

/// Mirrors the progress on the taskbar button, like Explorer's copy dialog.
pub struct Taskbar {
    list: Option<ITaskbarList3>,
    hwnd: HWND,
    state: TaskbarState,
}

impl Taskbar {
    pub fn new(hwnd: HWND) -> Self {
        let list = unsafe { CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER) }
            .ok()
            .filter(|list| unsafe { list.HrInit() }.is_ok());
        Taskbar { list, hwnd, state: TaskbarState::None }
    }

    pub fn set(&mut self, state: TaskbarState, value: f64) {
        let Some(list) = &self.list else { return };
        let hwnd = WinHwnd(self.hwnd as _);
        let flag: TBPFLAG = match state {
            TaskbarState::None => TBPF_NOPROGRESS,
            TaskbarState::Indeterminate => TBPF_INDETERMINATE,
            TaskbarState::Normal => TBPF_NORMAL,
            TaskbarState::Paused => TBPF_PAUSED,
            TaskbarState::Error => TBPF_ERROR,
        };
        unsafe {
            if state != self.state {
                let _ = list.SetProgressState(hwnd, flag);
                self.state = state;
            }
            if matches!(state, TaskbarState::Normal | TaskbarState::Paused | TaskbarState::Error) {
                let _ = list.SetProgressValue(hwnd, (value.clamp(0.0, 1.0) * 10000.0) as u64, 10000);
            }
        }
    }
}
