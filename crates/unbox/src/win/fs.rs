//! Handle-pinned file system access. Every object Unbox creates or verifies is bound
//! to the directory it was supposed to be in by comparing the final path of its
//! handle, so junctions, symbolic links or concurrently swapped ancestors can never
//! redirect a write or a verification outside the staging area.

use super::{from_wide, full_path_name, wide, Handle};
use std::ffi::c_void;
use std::fs::File;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use unbox_core::sanitize::{is_under_root, join_below, strip_extended_prefix};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;

const FILE_LIST_DIRECTORY_ACCESS: u32 = 0x0001;
const FILE_TRAVERSE_ACCESS: u32 = 0x0020;
const FILE_READ_ATTRIBUTES_ACCESS: u32 = 0x0080;
const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
const DELETE_ACCESS: u32 = 0x0001_0000;

/// Final path of an open handle without the `\\?\` prefix and trailing separator.
pub fn final_path(handle: HANDLE) -> Option<String> {
    let mut buffer = vec![0u16; 1024];
    for _ in 0..4 {
        let len =
            unsafe { GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS) };
        if len == 0 {
            return None;
        }
        if (len as usize) < buffer.len() {
            let path = strip_extended_prefix(&from_wide(&buffer[..len as usize]));
            return Some(path.trim_end_matches('\\').to_string());
        }
        buffer.resize(len as usize + 1, 0);
    }
    None
}

/// File attributes via the handle; `None` when they cannot be read.
pub fn attributes(handle: HANDLE) -> Option<u32> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    (unsafe { GetFileInformationByHandle(handle, &mut info) } != 0).then_some(info.dwFileAttributes)
}

/// Fail closed: unreadable attributes count as a reparse point.
pub fn is_reparse(handle: HANDLE) -> bool {
    attributes(handle).is_none_or(|a| a & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

fn open(path: &str, access: u32, share: u32, flags: u32) -> Option<Handle> {
    let handle = unsafe { CreateFileW(wide(path).as_ptr(), access, share, std::ptr::null(), OPEN_EXISTING, flags, std::ptr::null_mut()) };
    (handle != INVALID_HANDLE_VALUE).then_some(Handle(handle))
}

/// Open a file or directory without following a reparse point at the leaf, so the
/// caller can detect it.
pub fn open_no_follow(path: &str) -> Option<Handle> {
    open(
        path,
        FILE_READ_ATTRIBUTES_ACCESS | FILE_TRAVERSE_ACCESS,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
    )
}

/// Open an object (file or directory) following reparse points, so its final path
/// reveals any redirection. Not sharing delete keeps it from being renamed while
/// pinned; `allow_write = false` also refuses concurrent writers (digest checks).
pub fn open_pinned(path: &str, allow_write: bool) -> Option<Handle> {
    let share = FILE_SHARE_READ | if allow_write { FILE_SHARE_WRITE } else { 0 };
    open(path, GENERIC_READ, share, FILE_FLAG_BACKUP_SEMANTICS)
}

/// Resolve a directory and require its lexical and physical location to agree.
/// Returns the canonical path with a trailing separator.
pub fn resolve_canonical_directory(dir: &str) -> Option<String> {
    let handle = open_no_follow(dir)?;
    if is_reparse(handle.raw()) {
        return None;
    }
    let physical = final_path(handle.raw())?;
    let lexical = strip_extended_prefix(&full_path_name(dir.as_ref())?).trim_end_matches('\\').to_string();
    if !is_under_root(&lexical, &physical) || !is_under_root(&physical, &lexical) {
        return None;
    }
    Some(format!("{physical}\\"))
}

/// Directory handles kept open while a child is created below them.
#[derive(Default)]
pub struct PinnedDirectories(Vec<Handle>);

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: HANDLE,
    object_name: *mut UnicodeString,
    attributes: u32,
    security_descriptor: *mut c_void,
    security_quality_of_service: *mut c_void,
}

#[repr(C)]
struct IoStatusBlock {
    status: usize,
    information: usize,
}

#[link(name = "ntdll", kind = "raw-dylib")]
extern "system" {
    fn NtCreateFile(
        file_handle: *mut HANDLE,
        desired_access: u32,
        object_attributes: *mut ObjectAttributes,
        io_status_block: *mut IoStatusBlock,
        allocation_size: *mut i64,
        file_attributes: u32,
        share_access: u32,
        create_disposition: u32,
        create_options: u32,
        ea_buffer: *mut c_void,
        ea_length: u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}

/// Create (or open) every directory from `canonical_root` down to `full_path`, one
/// name at a time relative to the verified parent handle, rejecting any reparse point.
/// The handles stay pinned in `pinned` until the caller drops it.
pub fn ensure_directory(canonical_root: &str, full_path: &str, pinned: &mut PinnedDirectories) -> Result<(), u32> {
    let root = strip_extended_prefix(canonical_root).trim_end_matches('\\').to_string();
    let target = strip_extended_prefix(full_path).trim_end_matches('\\').to_string();
    if !is_under_root(&root, &target) {
        return Err(ERROR_ACCESS_DENIED);
    }
    let root_handle =
        open(&root, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .ok_or_else(super::last_error)?;
    let root_final = final_path(root_handle.raw());
    pinned.0.push(root_handle);
    if is_reparse(pinned.0.last().unwrap().raw()) || !root_final.is_some_and(|f| unbox_core::eq_ignore_case(&f, &root)) {
        return Err(ERROR_ACCESS_DENIED);
    }
    if target.len() <= root.len() {
        return Ok(());
    }
    for name in target[root.len() + 1..].split('\\') {
        if name.is_empty() || name == "." || name == ".." || name.contains([':', '/']) || name.len() > 32767 {
            return Err(ERROR_ACCESS_DENIED);
        }
        let mut name_wide: Vec<u16> = name.encode_utf16().collect();
        let mut unicode = UnicodeString {
            length: (name_wide.len() * 2) as u16,
            maximum_length: (name_wide.len() * 2) as u16,
            buffer: name_wide.as_mut_ptr(),
        };
        let mut attrs = ObjectAttributes {
            length: std::mem::size_of::<ObjectAttributes>() as u32,
            root_directory: pinned.0.last().unwrap().raw(),
            object_name: &mut unicode,
            attributes: 0x40, // OBJ_CASE_INSENSITIVE
            security_descriptor: std::ptr::null_mut(),
            security_quality_of_service: std::ptr::null_mut(),
        };
        let mut io = IoStatusBlock { status: 0, information: 0 };
        let mut child: HANDLE = std::ptr::null_mut();
        let status = unsafe {
            NtCreateFile(
                &mut child,
                FILE_LIST_DIRECTORY_ACCESS | FILE_TRAVERSE_ACCESS | FILE_READ_ATTRIBUTES_ACCESS | SYNCHRONIZE_ACCESS,
                &mut attrs,
                &mut io,
                std::ptr::null_mut(),
                FILE_ATTRIBUTE_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                3,                        // FILE_OPEN_IF
                0x1 | 0x20 | 0x0020_0000, // DIRECTORY_FILE | SYNCHRONOUS_IO_NONALERT | OPEN_REPARSE_POINT
                std::ptr::null_mut(),
                0,
            )
        };
        if status < 0 {
            return Err(unsafe { RtlNtStatusToDosError(status) });
        }
        pinned.0.push(Handle(child));
        if is_reparse(child) || !final_path(child).is_some_and(|f| is_under_root(&root, &f)) {
            return Err(ERROR_ACCESS_DENIED);
        }
    }
    Ok(())
}

/// Lexical first gate: join `rel` below `root` and require the Win32-normalised
/// result to stay below the normalised root.
pub fn resolve_destination(root: &str, rel: &str) -> Option<String> {
    let canon_root = full_path_name(root.as_ref())?;
    let joined = join_below(&canon_root, rel)?;
    let canon_target = full_path_name(joined.as_ref())?;
    let root_with_sep = format!("{}\\", canon_root.trim_end_matches('\\'));
    (canon_target.len() >= root_with_sep.len() && unbox_core::starts_with_ignore_case(&canon_target, &root_with_sep))
        .then_some(canon_target)
}

/// Create a new output file exclusively. Name collisions get ` (2)`, ` (3)` …; the
/// final path of the handle must stay below `canonical_root`, otherwise the file is
/// deleted again. Returns the file and its actual path.
pub fn create_output_file(path: &str, canonical_root: &str) -> Result<(File, String), u32> {
    let slash = path.rfind(['\\', '/']);
    let (base, ext) = match path.rfind('.') {
        Some(dot) if slash.is_none_or(|s| dot > s) => (&path[..dot], &path[dot..]),
        _ => (path, ""),
    };
    let mut last_error = 0;
    for index in 1..=999 {
        let candidate = if index == 1 { path.to_string() } else { format!("{base} ({index}){ext}") };
        let result = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .access_mode(GENERIC_WRITE | DELETE_ACCESS | FILE_READ_ATTRIBUTES_ACCESS)
            .share_mode(0)
            .open(&candidate);
        match result {
            Ok(file) => {
                let handle = file.as_raw_handle() as HANDLE;
                if !final_path(handle).is_some_and(|f| is_under_root(canonical_root, &f)) {
                    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
                    unsafe {
                        SetFileInformationByHandle(
                            handle,
                            FileDispositionInfo,
                            &disposition as *const _ as *const c_void,
                            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
                        );
                    }
                    return Err(ERROR_ACCESS_DENIED);
                }
                return Ok((file, candidate));
            }
            Err(error) => {
                last_error = error.raw_os_error().unwrap_or(0) as u32;
                if last_error != ERROR_FILE_EXISTS && last_error != ERROR_ALREADY_EXISTS {
                    break;
                }
            }
        }
    }
    Err(if last_error == 0 { ERROR_ACCESS_DENIED } else { last_error })
}

/// Open an input file for reading. With `reject_reparse`, links and directories are
/// refused (used for volumes requested by the 7-Zip handler).
pub fn open_input(path: &str, reject_reparse: bool) -> Option<File> {
    let flags = if reject_reparse { FILE_FLAG_OPEN_REPARSE_POINT } else { FILE_ATTRIBUTE_NORMAL };
    let handle = unsafe {
        CreateFileW(wide(path).as_ptr(), GENERIC_READ, FILE_SHARE_READ, std::ptr::null(), OPEN_EXISTING, flags, std::ptr::null_mut())
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let file = unsafe { File::from_raw_handle(handle as _) };
    if reject_reparse {
        let attrs = attributes(handle)?;
        if attrs & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            return None;
        }
    }
    Some(file)
}
