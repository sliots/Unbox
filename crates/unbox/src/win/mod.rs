//! Thin, safe wrappers over the Win32 APIs Unbox needs.

pub mod fs;
pub mod job;

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use unbox_core::decode::CodePages;
use unbox_core::ipc::VolumeIdentity;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Globalization::*;
use windows_sys::Win32::Storage::FileSystem::*;

/// NUL-terminated UTF-16.
pub fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

pub fn from_wide(value: &[u16]) -> String {
    OsString::from_wide(value).to_string_lossy().into_owned()
}

/// Read a NUL-terminated UTF-16 string.
///
/// # Safety
/// `ptr` must be null or point to a NUL-terminated buffer.
pub unsafe fn from_wide_ptr(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    from_wide(std::slice::from_raw_parts(ptr, len))
}

pub fn last_error() -> u32 {
    unsafe { GetLastError() }
}

pub fn hresult_from_win32(code: u32) -> i32 {
    if code == 0 {
        0
    } else {
        ((code & 0xFFFF) | 0x8007_0000) as i32
    }
}

/// Let `unbox register` print to the terminal it was started from.
/// Redirected handles (pipes, files) are kept as they are.
pub fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE};
    unsafe {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        if out.is_null() || out == INVALID_HANDLE_VALUE {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

/// Owned Win32 handle closed on drop.
pub struct Handle(pub HANDLE);

impl Handle {
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

// The handles are plain kernel objects; moving them between threads is fine.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

/// `GetFullPathNameW`.
pub fn full_path_name(path: &OsStr) -> Option<String> {
    let input = wide(path);
    let mut buffer = vec![0u16; 1024];
    loop {
        let len = unsafe { GetFullPathNameW(input.as_ptr(), buffer.len() as u32, buffer.as_mut_ptr(), std::ptr::null_mut()) };
        if len == 0 {
            return None;
        }
        if (len as usize) < buffer.len() {
            return Some(from_wide(&buffer[..len as usize]));
        }
        buffer.resize(len as usize + 1, 0);
    }
}

/// Free bytes available to the caller on the volume holding `directory`.
pub fn disk_free(directory: &Path) -> Option<u64> {
    let mut dir = directory.as_os_str().to_owned();
    if !dir.to_string_lossy().ends_with('\\') {
        dir.push("\\");
    }
    let mut free = 0u64;
    let ok = unsafe { GetDiskFreeSpaceExW(wide(&dir).as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(free)
}

/// Capture the object identity of `path` (volume serial, 128-bit file id, size, mtime).
pub fn capture_identity(path: &str) -> VolumeIdentity {
    let mut identity = VolumeIdentity { path: path.to_string(), ..Default::default() };
    let handle = unsafe {
        CreateFileW(
            wide(path).as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return identity;
    }
    let handle = Handle(handle);
    let mut id_info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let mut ok = unsafe {
        GetFileInformationByHandleEx(handle.raw(), FileIdInfo, &mut id_info as *mut _ as *mut _, std::mem::size_of::<FILE_ID_INFO>() as u32)
    } != 0;
    if ok {
        identity.volume_serial = id_info.VolumeSerialNumber;
        identity.file_id = id_info.FileId.Identifier;
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle.raw(), &mut info) } != 0 {
        identity.size = ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64;
        identity.last_write_time = ((info.ftLastWriteTime.dwHighDateTime as u64) << 32) | info.ftLastWriteTime.dwLowDateTime as u64;
    } else {
        ok = false;
    }
    identity.valid = ok;
    identity
}

/// `MultiByteToWideChar` based decoder.
pub struct WinCodePages;

impl CodePages for WinCodePages {
    fn decode(&self, bytes: &[u8], cp: u32) -> (String, bool) {
        if bytes.is_empty() {
            return (String::new(), true);
        }
        let cp = if cp == 0 { CP_ACP } else { cp };
        let convert =
            |flags: u32, out: *mut u16, len: i32| unsafe { MultiByteToWideChar(cp, flags, bytes.as_ptr(), bytes.len() as i32, out, len) };
        let strict = convert(MB_ERR_INVALID_CHARS, std::ptr::null_mut(), 0);
        let clean = strict > 0;
        let flags = if clean { MB_ERR_INVALID_CHARS } else { 0 };
        let len = if clean { strict } else { convert(0, std::ptr::null_mut(), 0) };
        if len <= 0 {
            return (String::new(), false);
        }
        let mut buffer = vec![0u16; len as usize];
        let written = convert(flags, buffer.as_mut_ptr(), len);
        buffer.truncate(written.max(0) as usize);
        (from_wide(&buffer), clean)
    }
}

/// The best default code page for legacy archive names. Windows 11 can run with the
/// global "UTF-8 for worldwide language support" option, in which case the user
/// locale's ANSI code page is a better guess for archives made by other machines.
pub fn default_archive_code_page() -> u32 {
    let acp = unsafe { GetACP() };
    if acp != 0 && acp != CP_UTF8 {
        return acp;
    }
    for locale in [LOCALE_USER_DEFAULT, LOCALE_SYSTEM_DEFAULT] {
        let mut buffer = [0u16; 16];
        let len = unsafe { GetLocaleInfoW(locale, LOCALE_IDEFAULTANSICODEPAGE, buffer.as_mut_ptr(), buffer.len() as i32) };
        if len > 0 {
            let text = from_wide(&buffer[..(len as usize).saturating_sub(1)]);
            if let Ok(cp) = text.trim().parse::<u32>() {
                if cp != 0 && cp != CP_UTF8 {
                    return cp;
                }
            }
        }
    }
    936
}

/// Map `X:\…` on a network drive to its UNC form so that paths compare equal to the
/// final paths returned by file handles.
pub fn resolve_network_drive(full: &str) -> String {
    use windows_sys::Win32::NetworkManagement::WNet::WNetGetConnectionW;
    let bytes = full.as_bytes();
    if bytes.len() < 3 || bytes[1] != b':' || bytes[2] != b'\\' {
        return full.to_string();
    }
    let root = &full[..3];
    if unsafe { GetDriveTypeW(wide(root).as_ptr()) } != 4 {
        // DRIVE_REMOTE
        return full.to_string();
    }
    let mut capacity = 32768u32;
    let mut buffer = vec![0u16; capacity as usize];
    let rc = unsafe { WNetGetConnectionW(wide(&full[..2]).as_ptr(), buffer.as_mut_ptr(), &mut capacity) };
    if rc != 0 {
        return full.to_string();
    }
    let len = buffer.iter().position(|c| *c == 0).unwrap_or(0);
    let remote = from_wide(&buffer[..len]);
    if remote.starts_with("\\\\") {
        format!("{}\\{}", remote.trim_end_matches('\\'), &full[3..])
    } else {
        full.to_string()
    }
}

/// Path text without the `\\?\` prefix that `canonicalize` adds.
pub fn fs_strip(path: &Path) -> String {
    unbox_core::sanitize::strip_extended_prefix(&path.to_string_lossy())
}

/// Put `text` on the clipboard as Unicode text.
pub fn copy_to_clipboard(text: &str) -> bool {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock};
    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 2;
    let data = wide(text);
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        EmptyClipboard();
        let memory = GlobalAlloc(GMEM_MOVEABLE, data.len() * 2);
        let mut ok = false;
        if !memory.is_null() {
            let target = GlobalLock(memory) as *mut u16;
            if !target.is_null() {
                std::ptr::copy_nonoverlapping(data.as_ptr(), target, data.len());
                GlobalUnlock(memory);
                ok = !SetClipboardData(CF_UNICODETEXT, memory as _).is_null();
            }
            if !ok {
                GlobalFree(memory);
            }
        }
        CloseClipboard();
        ok
    }
}
