//! Recycling the source volumes through the Shell (`IFileOperation`) with a progress
//! sink, so each volume's real result and recycle-bin location is reported. Before a
//! volume is handed to the Shell its object identity is checked again; a replaced or
//! modified file is never recycled.

#![allow(non_snake_case)]

use crate::win::{capture_identity, from_wide, wide, Handle};
use std::collections::HashMap;
use std::sync::Mutex;
use unbox_core::ipc::VolumeIdentity;
use windows::core::{implement, Ref, Result as WinResult, HRESULT, PCWSTR};
use windows::Win32::System::Com::*;
use windows::Win32::UI::Shell::*;
use winreg::enums::*;
use winreg::RegKey;

pub const BIN_DISABLED: &str = "回收站没有启用，源文件未删除";
const CAPACITY_UNKNOWN: &str = "无法确认目标卷的回收站容量，已保留源文件。请检查回收站属性或联系管理员";
const CAPACITY_INSUFFICIENT: &str = "回收站剩余容量不足以容纳本次源文件，已保留源文件";
const E_ABORT: HRESULT = HRESULT(0x8000_4004_u32 as i32);
const MIB: i64 = 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct RecycleItem {
    pub path: String,
    pub recycled: bool,
    pub bin_unavailable: bool,
    pub detail: String,
    pub recycled_to: String,
}

#[derive(Clone, Debug, Default)]
pub struct RecycleResult {
    pub success: bool,
    pub message: String,
    pub items: Vec<RecycleItem>,
}

impl RecycleResult {
    pub fn failed(message: String) -> Self {
        RecycleResult { success: false, message, items: Vec::new() }
    }
}

fn fail(path: &str, detail: impl Into<String>, bin_unavailable: bool) -> RecycleItem {
    RecycleItem { path: path.into(), recycled: false, bin_unavailable, detail: detail.into(), recycled_to: String::new() }
}

#[derive(Clone, Debug)]
struct RecycleVolume {
    id: String,
    mount: String,
    capacity: Option<i64>,
    occupied: i64,
}

fn reg_dword(key: &Option<RegKey>, name: &str) -> Option<u32> {
    key.as_ref().and_then(|k| k.get_value::<u32, _>(name).ok())
}

fn has_value(key: &Option<RegKey>, name: &str) -> bool {
    key.as_ref().is_some_and(|k| k.get_raw_value(name).is_ok())
}

/// The recycle bin of the volume holding `path`, if it can provably take the file.
fn query_volume(path: &str) -> Option<RecycleVolume> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW};
    let bytes = path.as_bytes();
    if path.starts_with("\\\\") || bytes.len() < 3 || bytes[1] != b':' || bytes[2] != b'\\' {
        return None;
    }
    let mut mount = vec![0u16; 32768];
    if unsafe { GetVolumePathNameW(wide(path).as_ptr(), mount.as_mut_ptr(), mount.len() as u32) } == 0 {
        return None;
    }
    let mount = from_wide(&mount[..mount.iter().position(|c| *c == 0).unwrap_or(0)]);
    let drive_type = unsafe { GetDriveTypeW(wide(&mount).as_ptr()) };
    if drive_type != 2 && drive_type != 3 {
        return None; // Only removable and fixed drives have a recycle bin.
    }
    let mut volume = vec![0u16; 64];
    if unsafe { GetVolumeNameForVolumeMountPointW(wide(&mount).as_ptr(), volume.as_mut_ptr(), volume.len() as u32) } == 0 {
        return None;
    }
    let volume = from_wide(&volume[..volume.iter().position(|c| *c == 0).unwrap_or(0)]);
    let id = volume.trim_end_matches('\\').strip_prefix("\\\\?\\Volume")?.to_string();
    if !(id.starts_with('{') && id.ends_with('}') && id.len() == 38) {
        return None;
    }

    const POLICY: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
    let user_policy = RegKey::predef(HKEY_CURRENT_USER).open_subkey(POLICY).ok();
    let machine_policy = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(POLICY).ok();
    let bin = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{id}"))
        .ok();
    let configured = reg_dword(&bin, "MaxCapacity");
    if reg_dword(&user_policy, "NoRecycleFiles").unwrap_or(0) != 0
        || reg_dword(&machine_policy, "NoRecycleFiles").unwrap_or(0) != 0
        || reg_dword(&bin, "NukeOnDelete").unwrap_or(0) != 0
        || configured == Some(0)
    {
        return None;
    }
    let mut info = SHQUERYRBINFO { cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32, ..Default::default() };
    let mount_w = wide(&mount);
    if unsafe { SHQueryRecycleBinW(PCWSTR(mount_w.as_ptr()), &mut info) }.is_err() || info.i64Size < 0 {
        return None;
    }
    // SHQueryRecycleBin reports usage, not the quota. Policy can override the
    // per-volume setting, in which case the quota is unknown.
    let capacity = configured
        .filter(|c| *c > 0 && !has_value(&user_policy, "RecycleBinSize") && !has_value(&machine_policy, "RecycleBinSize"))
        .map(|c| c as i64 * MIB);
    Some(RecycleVolume { id, mount, capacity, occupied: info.i64Size })
}

fn check_capacity(bin: &RecycleVolume, pending: i64) -> Option<&'static str> {
    match bin.capacity {
        None => Some(CAPACITY_UNKNOWN),
        Some(capacity) if pending > capacity - bin.occupied => Some(CAPACITY_INSUFFICIENT),
        _ => None,
    }
}

fn user_sid() -> Option<String> {
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let token = Handle(token);
        let mut buffer = vec![0u8; 256];
        let mut len = 0u32;
        if GetTokenInformation(token.raw(), TokenUser, buffer.as_mut_ptr() as _, buffer.len() as u32, &mut len) == 0 {
            return None;
        }
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut text = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return None;
        }
        let sid = crate::win::from_wide_ptr(text);
        windows_sys::Win32::Foundation::LocalFree(text as _);
        Some(sid)
    }
}

/// Serialise recycling across Unbox processes of the same user.
struct RecycleLock(Handle);

impl RecycleLock {
    fn acquire() -> Option<Self> {
        use windows_sys::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        let name = format!("Global\\Unbox.Recycle.{}", user_sid()?);
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide(&name).as_ptr()) };
        if handle.is_null() {
            return None;
        }
        let handle = Handle(handle);
        match unsafe { WaitForSingleObject(handle.raw(), 5000) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Some(RecycleLock(handle)),
            _ => None,
        }
    }
}

impl Drop for RecycleLock {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Threading::ReleaseMutex(self.0.raw());
        }
    }
}

/// Find the recycled object by identity below `<mount>\$Recycle.Bin\<sid>`. A recycled
/// file keeps its file id because it is renamed within the volume.
fn locate_in_bin(expected: &VolumeIdentity, mount: &str) -> String {
    let Some(sid) = user_sid() else { return String::new() };
    let dir = format!("{}\\$Recycle.Bin\\{sid}", mount.trim_end_matches('\\'));
    let Ok(entries) = std::fs::read_dir(&dir) else { return String::new() };
    for entry in entries.filter_map(|e| e.ok()).take(20000) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("$R") {
            continue;
        }
        let path = format!("{dir}\\{name}");
        let current = capture_identity(&path);
        if expected.matches(&VolumeIdentity { path: expected.path.clone(), ..current }) {
            return path;
        }
    }
    String::new()
}

fn matches_destination(expected: &VolumeIdentity, path: &str) -> bool {
    !path.is_empty() && expected.matches(&VolumeIdentity { path: expected.path.clone(), ..capture_identity(path) })
}

#[derive(Default)]
struct SinkState {
    delete_result: Option<HRESULT>,
    destination: String,
    blocked: Option<String>,
}

#[implement(IFileOperationProgressSink)]
struct RecycleSink {
    expected: VolumeIdentity,
    volume_id: String,
    pending: i64,
    state: std::sync::Arc<Mutex<SinkState>>,
}

fn parsing_name(item: &IShellItem) -> Option<String> {
    unsafe {
        let name = item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING).ok()?;
        let text = crate::win::from_wide_ptr(name.0);
        CoTaskMemFree(Some(name.0 as _));
        Some(text)
    }
}

impl IFileOperationProgressSink_Impl for RecycleSink_Impl {
    fn StartOperations(&self) -> WinResult<()> {
        Ok(())
    }
    fn FinishOperations(&self, _hr: HRESULT) -> WinResult<()> {
        Ok(())
    }
    fn PreRenameItem(&self, _f: u32, _i: Ref<IShellItem>, _n: &PCWSTR) -> WinResult<()> {
        Ok(())
    }
    fn PostRenameItem(&self, _f: u32, _i: Ref<IShellItem>, _n: &PCWSTR, _hr: HRESULT, _c: Ref<IShellItem>) -> WinResult<()> {
        Ok(())
    }
    fn PreMoveItem(&self, _f: u32, _i: Ref<IShellItem>, _d: Ref<IShellItem>, _n: &PCWSTR) -> WinResult<()> {
        Ok(())
    }
    fn PostMoveItem(
        &self,
        _f: u32,
        _i: Ref<IShellItem>,
        _d: Ref<IShellItem>,
        _n: &PCWSTR,
        _hr: HRESULT,
        _c: Ref<IShellItem>,
    ) -> WinResult<()> {
        Ok(())
    }
    fn PreCopyItem(&self, _f: u32, _i: Ref<IShellItem>, _d: Ref<IShellItem>, _n: &PCWSTR) -> WinResult<()> {
        Ok(())
    }
    fn PostCopyItem(
        &self,
        _f: u32,
        _i: Ref<IShellItem>,
        _d: Ref<IShellItem>,
        _n: &PCWSTR,
        _hr: HRESULT,
        _c: Ref<IShellItem>,
    ) -> WinResult<()> {
        Ok(())
    }
    fn PreDeleteItem(&self, flags: u32, _item: Ref<IShellItem>) -> WinResult<()> {
        const TSF_DELETE_RECYCLE_IF_POSSIBLE: u32 = 0x80;
        let blocked = if flags & TSF_DELETE_RECYCLE_IF_POSSIBLE == 0 {
            Some(BIN_DISABLED.to_string())
        } else {
            match query_volume(&self.expected.path) {
                None => Some(BIN_DISABLED.to_string()),
                Some(bin) if bin.id != self.volume_id => Some("源文件所在卷已改变，已保留源文件".to_string()),
                Some(bin) => check_capacity(&bin, self.pending).map(str::to_string),
            }
        }
        .or_else(|| {
            (!self.expected.matches(&capture_identity(&self.expected.path))).then(|| "源文件身份已改变或无法核对，已保留源文件".to_string())
        });
        let abort = blocked.is_some();
        self.state.lock().unwrap().blocked = blocked;
        if abort {
            Err(E_ABORT.into())
        } else {
            Ok(())
        }
    }
    fn PostDeleteItem(&self, _f: u32, _item: Ref<IShellItem>, hr: HRESULT, created: Ref<IShellItem>) -> WinResult<()> {
        let mut state = self.state.lock().unwrap();
        state.delete_result = Some(hr);
        state.destination = created.as_ref().and_then(parsing_name).unwrap_or_default();
        Ok(())
    }
    fn PreNewItem(&self, _f: u32, _d: Ref<IShellItem>, _n: &PCWSTR) -> WinResult<()> {
        Ok(())
    }
    fn PostNewItem(
        &self,
        _f: u32,
        _d: Ref<IShellItem>,
        _n: &PCWSTR,
        _t: &PCWSTR,
        _a: u32,
        _hr: HRESULT,
        _i: Ref<IShellItem>,
    ) -> WinResult<()> {
        Ok(())
    }
    fn UpdateProgress(&self, _total: u32, _done: u32) -> WinResult<()> {
        Ok(())
    }
    fn ResetTimer(&self) -> WinResult<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> WinResult<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> WinResult<()> {
        Ok(())
    }
}

fn recycle_one(expected: &VolumeIdentity, path: &str, bin: &RecycleVolume, pending: i64) -> RecycleItem {
    let state = std::sync::Arc::new(Mutex::new(SinkState::default()));
    let attempt = || -> WinResult<HRESULT> {
        unsafe {
            let operation: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
            operation.SetOperationFlags(FILEOPERATION_FLAGS(
                FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_SILENT.0 | FOF_NOERRORUI.0 | FOFX_RECYCLEONDELETE.0,
            ))?;
            let sink: IFileOperationProgressSink =
                RecycleSink { expected: expected.clone(), volume_id: bin.id.clone(), pending, state: state.clone() }.into();
            let path_w = wide(path);
            let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(path_w.as_ptr()), None)?;
            operation.DeleteItem(&item, &sink)?;
            let hr = match operation.PerformOperations() {
                Ok(()) => HRESULT(0),
                Err(e) => e.code(),
            };
            let aborted = operation.GetAnyOperationsAborted().map(|b| b.as_bool()).unwrap_or(true);
            Ok(if aborted && hr.is_ok() { E_ABORT } else { hr })
        }
    };
    let hr = match attempt() {
        Ok(hr) => hr,
        Err(e) => return fail(path, format!("调用 Shell 回收接口失败：{}", e.message()), false),
    };
    let state = state.lock().unwrap();
    if std::path::Path::new(path).exists() {
        let detail = state.blocked.clone().unwrap_or_else(|| format!("Shell 未完成回收（0x{:08X}），源文件仍在原位", hr.0 as u32));
        let unavailable = state.blocked.as_deref() == Some(BIN_DISABLED);
        return fail(path, detail, unavailable);
    }
    if hr.is_err() || state.delete_result.is_none_or(|r| r.is_err()) {
        return fail(path, "源文件已不在原位，未能确认 Shell 回收结果，请检查回收站", false);
    }
    let mut destination = state.destination.clone();
    if !matches_destination(expected, &destination) {
        destination = locate_in_bin(expected, &bin.mount);
    }
    if destination.is_empty() {
        return fail(path, "源文件已不在原位，未能核实可还原的回收站落点，请检查回收站", false);
    }
    RecycleItem {
        path: path.into(),
        recycled: true,
        bin_unavailable: false,
        detail: format!("已移入回收站：{destination}"),
        recycled_to: destination,
    }
}

/// Recycle the volumes the worker actually read.
pub fn recycle_volumes(volumes: &[VolumeIdentity]) -> RecycleResult {
    if volumes.is_empty() {
        return RecycleResult { success: true, message: "无需回收".into(), items: Vec::new() };
    }
    let _com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let Some(_lock) = RecycleLock::acquire() else {
        let reason = "无法独占本次 Unbox 回收操作，已保留源文件";
        return RecycleResult {
            success: false,
            message: reason.into(),
            items: volumes.iter().map(|v| fail(&v.path, reason, false)).collect(),
        };
    };

    let mut items = Vec::new();
    let mut verified: Vec<(VolumeIdentity, String, RecycleVolume)> = Vec::new();
    const MISSING_IDENTITY: &str = "缺少解压时记录的源文件身份，无法确认未被替换，已保留";
    for volume in volumes {
        if volume.path.trim().is_empty() {
            items.push(fail(&volume.path, "源卷路径为空，已保留", false));
            continue;
        }
        let full = crate::win::full_path_name(volume.path.as_ref()).unwrap_or_else(|| volume.path.clone());
        if !std::path::Path::new(&full).is_file() {
            items.push(fail(&full, "源卷不存在或已被移除，已保留", false));
            continue;
        }
        let Some(bin) = query_volume(&full) else {
            let detail = if volume.valid { BIN_DISABLED.to_string() } else { format!("{BIN_DISABLED}；{MISSING_IDENTITY}") };
            items.push(fail(&full, detail, true));
            continue;
        };
        if !volume.valid {
            items.push(fail(&full, MISSING_IDENTITY, false));
            continue;
        }
        let current = capture_identity(&full);
        if !current.valid {
            items.push(fail(&full, "无法读取源文件对象身份，已保留", false));
            continue;
        }
        if !volume.matches(&current) {
            items.push(fail(&full, "源文件在解压后已被替换或修改，已保留", false));
            continue;
        }
        verified.push((volume.clone(), full, bin));
    }
    if verified.is_empty() {
        return RecycleResult { success: false, message: "没有可回收的源卷（全部已保留）".into(), items };
    }

    let mut pending: HashMap<String, i64> = HashMap::new();
    let mut preflight = items.iter().any(|i| i.bin_unavailable).then_some("本次源卷包含无法确认可回收的卷，已保留其余源文件");
    if preflight.is_none() {
        for (expected, _, bin) in &verified {
            let current = pending.get(&bin.id).copied().unwrap_or(0);
            let size = expected.size as i64;
            if size > i64::MAX - current {
                preflight = Some(CAPACITY_INSUFFICIENT);
                break;
            }
            if let Some(problem) = check_capacity(bin, current + size) {
                preflight = Some(problem);
                break;
            }
            pending.insert(bin.id.clone(), current + size);
        }
    }
    if let Some(reason) = preflight {
        items.extend(verified.iter().map(|(_, path, _)| fail(path, reason, false)));
        return RecycleResult { success: false, message: reason.into(), items };
    }

    let first = items.len();
    for (expected, path, bin) in &verified {
        let item = recycle_one(expected, path, bin, pending[&bin.id]);
        if item.recycled {
            *pending.get_mut(&bin.id).unwrap() -= expected.size as i64;
        }
        items.push(item);
    }

    // A later operation can change the bin again; count only items still found there.
    for (offset, (expected, path, bin)) in verified.iter().enumerate() {
        let index = first + offset;
        if !items[index].recycled {
            continue;
        }
        if std::path::Path::new(path).exists() {
            items[index] = fail(path, "任务结束时源路径仍有文件，请核对原路径和回收站", false);
            continue;
        }
        let destination = if matches_destination(expected, &items[index].recycled_to) {
            items[index].recycled_to.clone()
        } else {
            locate_in_bin(expected, &bin.mount)
        };
        if destination.is_empty() {
            items[index] = fail(path, "源文件已不在原位，任务结束时未能核实可还原的回收站落点，请检查回收站", false);
        } else {
            items[index].detail = format!("已移入回收站：{destination}");
            items[index].recycled_to = destination;
        }
    }

    let recycled = items.iter().filter(|i| i.recycled).count();
    let success = recycled == items.len();
    let message = if success {
        format!("已确认 {} 个源卷可从回收站还原", items.len())
    } else {
        format!("{recycled}/{} 个源卷已确认移入回收站", items.len())
    };
    RecycleResult { success, message, items }
}
