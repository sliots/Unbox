//! COM objects handed to 7z.dll: file streams, the open callback (volumes and header
//! passwords) and the extract callback (safe output creation and bookkeeping).

#![allow(non_snake_case)]

use super::control::{control, ArchivePassword};
use super::sevenzip::*;
use crate::win::fs::{create_output_file, ensure_directory, open_input, resolve_destination, PinnedDirectories};
use std::ffi::c_void;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use unbox_core::budget::OutputBudget;
use unbox_core::ipc::{Phase, VolumeIdentity, WorkerEvent};
use windows_core::{implement, Interface, HRESULT};

fn io_hr(error: &std::io::Error) -> HRESULT {
    error.raw_os_error().map(|c| hr_from_win32(c as u32)).unwrap_or(E_FAIL)
}

// ---------------------------------------------------------------------------
// Input stream
// ---------------------------------------------------------------------------

#[implement(IInStream, IStreamGetSize)]
pub struct InFileStream {
    file: Mutex<File>,
}

impl InFileStream {
    pub fn open(path: &str, reject_reparse: bool) -> Option<IInStream> {
        open_input(path, reject_reparse).map(|file| InFileStream { file: Mutex::new(file) }.into())
    }
}

impl ISequentialInStream_Impl for InFileStream_Impl {
    unsafe fn Read(&self, data: *mut c_void, size: u32, processed: *mut u32) -> HRESULT {
        if !processed.is_null() {
            *processed = 0;
        }
        if size == 0 {
            return S_OK;
        }
        let buffer = std::slice::from_raw_parts_mut(data as *mut u8, size as usize);
        match self.file.lock().unwrap().read(buffer) {
            Ok(n) => {
                if !processed.is_null() {
                    *processed = n as u32;
                }
                S_OK
            }
            Err(e) => io_hr(&e),
        }
    }
}

impl IInStream_Impl for InFileStream_Impl {
    unsafe fn Seek(&self, offset: i64, origin: u32, new_position: *mut u64) -> HRESULT {
        let target = match origin {
            0 if offset >= 0 => SeekFrom::Start(offset as u64),
            0 => return hr_from_win32(131), // ERROR_NEGATIVE_SEEK
            1 => SeekFrom::Current(offset),
            2 => SeekFrom::End(offset),
            _ => return E_INVALIDARG,
        };
        match self.file.lock().unwrap().seek(target) {
            Ok(position) => {
                if !new_position.is_null() {
                    *new_position = position;
                }
                S_OK
            }
            Err(e) => io_hr(&e),
        }
    }
}

impl IStreamGetSize_Impl for InFileStream_Impl {
    unsafe fn GetSize(&self, size: *mut u64) -> HRESULT {
        if size.is_null() {
            return E_POINTER;
        }
        match self.file.lock().unwrap().metadata() {
            Ok(meta) => {
                *size = meta.len();
                S_OK
            }
            Err(e) => io_hr(&e),
        }
    }
}

// ---------------------------------------------------------------------------
// Output stream
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct OutState {
    file: Option<File>,
    crc: crc32fast::Hasher,
    written: u64,
    write_error: u32,
}

#[implement(ISequentialOutStream)]
struct OutFileStream {
    state: Arc<Mutex<OutState>>,
    budget: Arc<OutputBudget>,
    on_written: Arc<dyn Fn(u64) + Send + Sync>,
}

impl ISequentialOutStream_Impl for OutFileStream_Impl {
    unsafe fn Write(&self, data: *const c_void, size: u32, processed: *mut u32) -> HRESULT {
        if !processed.is_null() {
            *processed = 0;
        }
        if !control().check() {
            return E_ABORT;
        }
        let mut state = self.state.lock().unwrap();
        if state.file.is_none() {
            return E_FAIL;
        }
        if !self.budget.consume(size as u64) {
            return E_ABORT; // Over budget: stop the task.
        }
        let buffer: &[u8] = if size == 0 { &[] } else { std::slice::from_raw_parts(data as *const u8, size as usize) };
        let result = state.file.as_mut().unwrap().write(buffer);
        let (written, hr) = match &result {
            Ok(n) => (*n, S_OK),
            Err(e) => {
                state.write_error = e.raw_os_error().unwrap_or(29) as u32; // ERROR_WRITE_FAULT
                (0, io_hr(e))
            }
        };
        self.budget.settle(size as u64, written as u64);
        if written > 0 {
            state.crc.update(&buffer[..written]);
            state.written += written as u64;
            drop(state);
            (self.on_written)(written as u64);
        }
        if !processed.is_null() {
            *processed = written as u32;
        }
        hr
    }
}

// ---------------------------------------------------------------------------
// Progress throttle
// ---------------------------------------------------------------------------

pub struct Progress {
    phase: Phase,
    size_known: bool,
    last: Mutex<Option<Instant>>,
}

impl Progress {
    pub fn new(phase: Phase, size_known: bool) -> Self {
        Self { phase, size_known, last: Mutex::new(None) }
    }

    fn report(&self, file: &str, completed: u64, total: u64) {
        let mut last = self.last.lock().unwrap();
        let now = Instant::now();
        let due = last.is_none_or(|t| now.duration_since(t).as_millis() >= 30);
        if due || (self.size_known && total > 0 && completed == total) {
            *last = Some(now);
            control().send(&WorkerEvent::Progress {
                phase: self.phase,
                current_file: file.to_string(),
                completed_bytes: completed,
                total_bytes: total,
                size_known: self.size_known,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Open callback
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct OpenState {
    pub opened_volumes: Vec<String>,
    pub identities: Vec<VolumeIdentity>,
}

#[implement(IArchiveOpenCallback, IArchiveOpenVolumeCallback, ICryptoGetTextPassword)]
pub struct OpenCallback {
    first_volume: String,
    password: Option<Arc<ArchivePassword>>,
    pub state: Arc<Mutex<OpenState>>,
}

impl OpenCallback {
    pub fn create(first_volume: &str, password: Option<Arc<ArchivePassword>>) -> (IArchiveOpenCallback, Arc<Mutex<OpenState>>) {
        let state = Arc::new(Mutex::new(OpenState::default()));
        {
            let mut s = state.lock().unwrap();
            s.opened_volumes.push(first_volume.to_string());
            let identity = crate::win::capture_identity(first_volume);
            if identity.valid {
                s.identities.push(identity);
            }
        }
        let callback = OpenCallback { first_volume: first_volume.to_string(), password, state: state.clone() };
        (callback.into(), state)
    }
}

impl IArchiveOpenCallback_Impl for OpenCallback_Impl {
    unsafe fn SetTotal(&self, _files: *const u64, _bytes: *const u64) -> HRESULT {
        S_OK
    }
    unsafe fn SetCompleted(&self, _files: *const u64, _bytes: *const u64) -> HRESULT {
        if control().canceled() {
            E_ABORT
        } else {
            S_OK
        }
    }
}

impl IArchiveOpenVolumeCallback_Impl for OpenCallback_Impl {
    unsafe fn GetProperty(&self, prop_id: u32, value: *mut PropVariant) -> HRESULT {
        if value.is_null() {
            return E_POINTER;
        }
        std::ptr::write(value, PropVariant::default());
        if prop_id == prop::NAME {
            let name = Path::new(&self.first_volume).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            (*value).set_string(&name);
        }
        S_OK
    }

    unsafe fn GetStream(&self, name: *const u16, stream: *mut *mut c_void) -> HRESULT {
        if name.is_null() || stream.is_null() {
            return E_POINTER;
        }
        *stream = std::ptr::null_mut();
        let leaf = crate::win::from_wide_ptr(name);
        if !unbox_core::sanitize::is_safe_relative_syntax(&leaf) || leaf == "." || leaf.contains(['\\', '/']) {
            return S_FALSE;
        }
        let dir = match self.first_volume.rfind(['\\', '/']) {
            Some(slash) => &self.first_volume[..=slash],
            None => "",
        };
        let full = format!("{dir}{leaf}");
        let Some(input) = InFileStream::open(&full, true) else { return S_FALSE };
        if unbox_core::naming::is_same_volume_family(Path::new(&self.first_volume), Path::new(&full)) {
            let mut state = self.state.lock().unwrap();
            if !state.opened_volumes.contains(&full) {
                state.opened_volumes.push(full.clone());
                let identity = crate::win::capture_identity(&full);
                if identity.valid {
                    state.identities.push(identity);
                }
            }
        }
        *stream = input.into_raw();
        S_OK
    }
}

unsafe fn give_password(password: &Option<Arc<ArchivePassword>>, out: *mut *mut u16) -> HRESULT {
    let Some(password) = password else { return E_ABORT };
    let Some(mut text) = password.get() else { return E_ABORT };
    *out = alloc_bstr(&text);
    text.iter_mut().for_each(|c| std::ptr::write_volatile(c, 0));
    if (*out).is_null() {
        E_OUTOFMEMORY
    } else {
        S_OK
    }
}

impl ICryptoGetTextPassword_Impl for OpenCallback_Impl {
    unsafe fn CryptoGetTextPassword(&self, password: *mut *mut u16) -> HRESULT {
        if password.is_null() {
            return E_POINTER;
        }
        *password = std::ptr::null_mut();
        give_password(&self.password, password)
    }
}

// ---------------------------------------------------------------------------
// Extract callback
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct DeliveredItem {
    /// Actual path relative to the extraction root (after collision renames).
    pub rel_disk_path: String,
    pub disk_path: String,
    pub is_dir: bool,
    pub size: u64,
    pub crc: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Disposition {
    Unseen,
    Delivered,
    UnsafeRejected,
    SkippedOrFiltered,
    Failed,
}

struct Current {
    index: u32,
    disk_path: String,
    is_dir: bool,
    out: Option<Arc<Mutex<OutState>>>,
}

#[derive(Default)]
pub struct ExtractOutcome {
    pub delivered: Vec<DeliveredItem>,
    pub unsafe_count: u32,
    pub has_error: bool,
    pub wrong_password: bool,
    pub crc_error: bool,
    pub data_error: bool,
    pub unsupported: bool,
    pub write_error: u32,
    pub missing: u32,
    pub completed_bytes: u64,
}

struct ExtractState {
    disposition: Vec<Disposition>,
    outcome: ExtractOutcome,
    current: Option<Current>,
}

pub struct ExtractShared {
    state: Mutex<ExtractState>,
    completed: AtomicU64,
    to_extract: Vec<bool>,
}

impl ExtractShared {
    /// Final bookkeeping once `Extract` has returned.
    pub fn finish(&self) -> ExtractOutcome {
        let mut state = self.state.lock().unwrap();
        if let Some(current) = state.current.take() {
            if let Some(out) = current.out {
                let error = out.lock().unwrap().write_error;
                if error != 0 {
                    state.outcome.write_error = error;
                }
            }
        }
        let missing = state
            .disposition
            .iter()
            .enumerate()
            .filter(|(i, d)| {
                self.to_extract.get(*i).copied().unwrap_or(false)
                    && !matches!(d, Disposition::Delivered | Disposition::UnsafeRejected | Disposition::Failed)
            })
            .count() as u32;
        let mut outcome = std::mem::take(&mut state.outcome);
        outcome.missing = missing;
        outcome.completed_bytes = self.completed.load(Ordering::Acquire);
        outcome
    }
}

pub struct ExtractConfig {
    pub archive: IInArchive,
    pub extraction_root: String,
    pub canonical_root: String,
    pub target_paths: Vec<String>,
    pub to_extract: Vec<bool>,
    pub total_bytes: u64,
    pub progress: Arc<Progress>,
    pub password: Option<Arc<ArchivePassword>>,
    pub budget: Arc<OutputBudget>,
}

#[implement(IArchiveExtractCallback, ICryptoGetTextPassword, ICryptoGetTextPassword2)]
pub struct ExtractCallback {
    config: ExtractConfig,
    shared: Arc<ExtractShared>,
}

impl ExtractCallback {
    pub fn create(config: ExtractConfig) -> (IArchiveExtractCallback, Arc<ExtractShared>) {
        let shared = Arc::new(ExtractShared {
            state: Mutex::new(ExtractState {
                disposition: vec![Disposition::Unseen; config.to_extract.len()],
                outcome: ExtractOutcome::default(),
                current: None,
            }),
            completed: AtomicU64::new(0),
            to_extract: config.to_extract.clone(),
        });
        (ExtractCallback { config, shared: shared.clone() }.into(), shared)
    }
}

fn relative_to(full: &str, root: &str) -> String {
    let root = format!("{}\\", root.trim_end_matches('\\'));
    if full.len() > root.len() && unbox_core::starts_with_ignore_case(full, &root) && full.is_char_boundary(root.len()) {
        full[root.len()..].to_string()
    } else {
        full.to_string()
    }
}

impl IArchiveExtractCallback_Impl for ExtractCallback_Impl {
    unsafe fn SetTotal(&self, _total: u64) -> HRESULT {
        // The engine may report packed or unpacked bytes; the declared plan total is used.
        S_OK
    }

    unsafe fn SetCompleted(&self, _complete: *const u64) -> HRESULT {
        if !control().check() || self.config.budget.exceeded() {
            E_ABORT
        } else {
            S_OK
        }
    }

    unsafe fn GetStream(&self, index: u32, stream: *mut *mut c_void, ask_mode: i32) -> HRESULT {
        if stream.is_null() {
            return E_POINTER;
        }
        *stream = std::ptr::null_mut();
        if !control().check() || self.config.budget.exceeded() {
            return E_ABORT;
        }
        let cfg = &self.config;
        let mut state = self.shared.state.lock().unwrap();
        state.current = None;
        let i = index as usize;
        let mark = |state: &mut ExtractState, d: Disposition| {
            if let Some(slot) = state.disposition.get_mut(i) {
                *slot = d;
            }
        };

        if i >= cfg.to_extract.len() || !cfg.to_extract[i] || ask_mode != ask::EXTRACT || i >= cfg.target_paths.len() {
            mark(&mut state, Disposition::SkippedOrFiltered);
            return S_OK;
        }
        let rel = &cfg.target_paths[i];
        let is_dir = item_property(&cfg.archive, index, prop::IS_DIR).as_bool();

        let Some(full) = resolve_destination(&cfg.extraction_root, rel) else {
            state.outcome.unsafe_count += 1;
            mark(&mut state, Disposition::UnsafeRejected);
            return S_OK;
        };

        if is_dir {
            let mut pinned = PinnedDirectories::default();
            if let Err(error) = ensure_directory(&cfg.canonical_root, &full, &mut pinned) {
                state.outcome.has_error = true;
                state.outcome.unsafe_count += 1;
                mark(&mut state, Disposition::UnsafeRejected);
                return hr_from_win32(if error == 0 { 5 } else { error });
            }
            state.current = Some(Current { index, disk_path: full, is_dir: true, out: None });
            return S_OK;
        }

        // Keep the verified parent chain pinned until the output handle is checked.
        let mut parents = PinnedDirectories::default();
        let parent = Path::new(&full).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        if let Err(error) = ensure_directory(&cfg.canonical_root, &parent, &mut parents) {
            state.outcome.has_error = true;
            state.outcome.unsafe_count += 1;
            mark(&mut state, Disposition::UnsafeRejected);
            return hr_from_win32(if error == 0 { 5 } else { error });
        }
        let (file, actual) = match create_output_file(&full, &cfg.canonical_root) {
            Ok(created) => created,
            Err(error) => {
                state.outcome.has_error = true;
                mark(&mut state, Disposition::Failed);
                return hr_from_win32(error);
            }
        };
        drop(parents);

        let out_state = Arc::new(Mutex::new(OutState { file: Some(file), ..Default::default() }));
        let shared = self.shared.clone();
        let progress = cfg.progress.clone();
        let total = cfg.total_bytes;
        let rel_for_progress = rel.clone();
        let out: ISequentialOutStream = OutFileStream {
            state: out_state.clone(),
            budget: cfg.budget.clone(),
            on_written: Arc::new(move |n| {
                let completed = shared.completed.fetch_add(n, Ordering::AcqRel) + n;
                progress.report(&rel_for_progress, completed, total);
            }),
        }
        .into();
        state.current = Some(Current { index, disk_path: actual, is_dir: false, out: Some(out_state) });
        *stream = out.into_raw();
        S_OK
    }

    unsafe fn PrepareOperation(&self, _ask_mode: i32) -> HRESULT {
        S_OK
    }

    unsafe fn SetOperationResult(&self, result: i32) -> HRESULT {
        let mut state = self.shared.state.lock().unwrap();
        let current = state.current.take();
        let root = &self.config.extraction_root;
        if let Some(current) = &current {
            let mut item = DeliveredItem {
                rel_disk_path: relative_to(&current.disk_path, root),
                disk_path: current.disk_path.clone(),
                is_dir: current.is_dir,
                size: 0,
                crc: 0,
            };
            if let Some(out) = &current.out {
                let mut out = out.lock().unwrap();
                out.file = None; // Close the handle now, whatever the engine still holds.
                item.size = out.written;
                item.crc = std::mem::take(&mut out.crc).finalize();
                if out.write_error != 0 {
                    state.outcome.write_error = out.write_error;
                }
            }
            state.outcome.delivered.push(item);
            if let Some(slot) = state.disposition.get_mut(current.index as usize) {
                *slot = if result == op_result::OK { Disposition::Delivered } else { Disposition::Failed };
            }
        }
        if result != op_result::OK {
            state.outcome.has_error = true;
            match result {
                op_result::WRONG_PASSWORD => {
                    state.outcome.wrong_password = true;
                    return E_ABORT;
                }
                op_result::CRC_ERROR => state.outcome.crc_error = true,
                op_result::DATA_ERROR => state.outcome.data_error = true,
                op_result::UNSUPPORTED_METHOD => state.outcome.unsupported = true,
                _ => {}
            }
        }
        S_OK
    }
}

impl ICryptoGetTextPassword_Impl for ExtractCallback_Impl {
    unsafe fn CryptoGetTextPassword(&self, password: *mut *mut u16) -> HRESULT {
        if password.is_null() {
            return E_POINTER;
        }
        *password = std::ptr::null_mut();
        give_password(&self.config.password, password)
    }
}

impl ICryptoGetTextPassword2_Impl for ExtractCallback_Impl {
    unsafe fn CryptoGetTextPassword2(&self, defined: *mut i32, password: *mut *mut u16) -> HRESULT {
        if defined.is_null() || password.is_null() {
            return E_POINTER;
        }
        *password = std::ptr::null_mut();
        *defined = 0;
        if self.config.password.is_none() {
            return S_OK;
        }
        let hr = give_password(&self.config.password, password);
        *defined = (!(*password).is_null()) as i32;
        hr
    }
}
