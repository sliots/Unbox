//! 7-Zip COM interfaces and the 7z.dll loader / format detector.

#![allow(non_snake_case, clippy::missing_safety_doc)]

use std::ffi::c_void;
use windows_core::{interface, IUnknown, IUnknown_Vtbl, Interface, GUID, HRESULT};

pub const S_OK: HRESULT = HRESULT(0);
pub const S_FALSE: HRESULT = HRESULT(1);
pub const E_ABORT: HRESULT = HRESULT(0x8000_4004_u32 as i32);
pub const E_FAIL: HRESULT = HRESULT(0x8000_4005_u32 as i32);
pub const E_POINTER: HRESULT = HRESULT(0x8000_4003_u32 as i32);
pub const E_INVALIDARG: HRESULT = HRESULT(0x8007_0057_u32 as i32);
pub const E_OUTOFMEMORY: HRESULT = HRESULT(0x8007_000E_u32 as i32);

pub fn hr_from_win32(code: u32) -> HRESULT {
    HRESULT(crate::win::hresult_from_win32(code))
}

// ---------------------------------------------------------------------------
// PROPVARIANT / BSTR (declared locally to keep the ABI explicit)
// ---------------------------------------------------------------------------

pub const VT_EMPTY: u16 = 0;
pub const VT_BOOL: u16 = 11;
pub const VT_BSTR: u16 = 8;
pub const VT_UI4: u16 = 19;
pub const VT_UI8: u16 = 21;

#[repr(C)]
pub struct PropVariant {
    pub vt: u16,
    pub reserved: [u16; 3],
    pub data: [u64; 2],
}

impl Default for PropVariant {
    fn default() -> Self {
        Self { vt: VT_EMPTY, reserved: [0; 3], data: [0; 2] }
    }
}

impl Drop for PropVariant {
    fn drop(&mut self) {
        unsafe {
            let _ = PropVariantClear(self);
        }
    }
}

impl PropVariant {
    fn bstr(&self) -> *const u16 {
        self.data[0] as usize as *const u16
    }

    pub fn as_string(&self) -> Option<String> {
        (self.vt == VT_BSTR && !self.bstr().is_null()).then(|| unsafe {
            let len = SysStringLen(self.bstr()) as usize;
            crate::win::from_wide(std::slice::from_raw_parts(self.bstr(), len))
        })
    }

    pub fn as_bytes(&self) -> Option<Vec<u8>> {
        (self.vt == VT_BSTR && !self.bstr().is_null()).then(|| unsafe {
            let len = SysStringByteLen(self.bstr()) as usize;
            std::slice::from_raw_parts(self.bstr() as *const u8, len).to_vec()
        })
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self.vt {
            VT_UI4 => Some(self.data[0] as u32 as u64),
            VT_UI8 => Some(self.data[0]),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> bool {
        self.vt == VT_BOOL && self.data[0] as u16 as i16 == -1
    }

    /// A non-empty string or a `true` boolean.
    pub fn is_present(&self) -> bool {
        (self.vt == VT_BSTR && !self.bstr().is_null() && unsafe { SysStringByteLen(self.bstr()) } != 0) || self.as_bool()
    }

    pub fn set_u32(&mut self, value: u32) {
        self.vt = VT_UI4;
        self.data[0] = value as u64;
    }

    /// Store `text` as a BSTR owned by the variant.
    pub fn set_string(&mut self, text: &str) {
        let wide: Vec<u16> = text.encode_utf16().collect();
        self.vt = VT_BSTR;
        self.data[0] = alloc_bstr(&wide) as usize as u64;
    }
}

#[link(name = "ole32", kind = "raw-dylib")]
extern "system" {
    fn PropVariantClear(value: *mut PropVariant) -> HRESULT;
}

#[link(name = "oleaut32", kind = "raw-dylib")]
extern "system" {
    fn SysAllocStringLen(text: *const u16, len: u32) -> *mut u16;
    fn SysStringLen(text: *const u16) -> u32;
    fn SysStringByteLen(text: *const u16) -> u32;
}

pub fn alloc_bstr(text: &[u16]) -> *mut u16 {
    unsafe { SysAllocStringLen(text.as_ptr(), text.len() as u32) }
}

// ---------------------------------------------------------------------------
// Interfaces (23170F69-40C1-278A-0000-00xx00yy0000)
// ---------------------------------------------------------------------------

#[interface("23170F69-40C1-278A-0000-000300010000")]
pub unsafe trait ISequentialInStream: IUnknown {
    pub unsafe fn Read(&self, data: *mut c_void, size: u32, processed: *mut u32) -> HRESULT;
}

#[interface("23170F69-40C1-278A-0000-000300020000")]
pub unsafe trait ISequentialOutStream: IUnknown {
    pub unsafe fn Write(&self, data: *const c_void, size: u32, processed: *mut u32) -> HRESULT;
}

#[interface("23170F69-40C1-278A-0000-000300030000")]
pub unsafe trait IInStream: ISequentialInStream {
    pub unsafe fn Seek(&self, offset: i64, origin: u32, new_position: *mut u64) -> HRESULT;
}

#[interface("23170F69-40C1-278A-0000-000300060000")]
pub unsafe trait IStreamGetSize: IUnknown {
    pub unsafe fn GetSize(&self, size: *mut u64) -> HRESULT;
}

#[interface("23170F69-40C1-278A-0000-000600100000")]
pub unsafe trait IArchiveOpenCallback: IUnknown {
    pub unsafe fn SetTotal(&self, files: *const u64, bytes: *const u64) -> HRESULT;
    pub unsafe fn SetCompleted(&self, files: *const u64, bytes: *const u64) -> HRESULT;
}

#[interface("23170F69-40C1-278A-0000-000600300000")]
pub unsafe trait IArchiveOpenVolumeCallback: IUnknown {
    pub unsafe fn GetProperty(&self, prop_id: u32, value: *mut PropVariant) -> HRESULT;
    pub unsafe fn GetStream(&self, name: *const u16, stream: *mut *mut c_void) -> HRESULT;
}

#[interface("23170F69-40C1-278A-0000-000600200000")]
pub unsafe trait IArchiveExtractCallback: IUnknown {
    pub unsafe fn SetTotal(&self, total: u64) -> HRESULT;
    pub unsafe fn SetCompleted(&self, complete: *const u64) -> HRESULT;
    pub unsafe fn GetStream(&self, index: u32, stream: *mut *mut c_void, ask_mode: i32) -> HRESULT;
    pub unsafe fn PrepareOperation(&self, ask_mode: i32) -> HRESULT;
    pub unsafe fn SetOperationResult(&self, result: i32) -> HRESULT;
}

#[interface("23170F69-40C1-278A-0000-000500100000")]
pub unsafe trait ICryptoGetTextPassword: IUnknown {
    pub unsafe fn CryptoGetTextPassword(&self, password: *mut *mut u16) -> HRESULT;
}

#[interface("23170F69-40C1-278A-0000-000500110000")]
pub unsafe trait ICryptoGetTextPassword2: IUnknown {
    pub unsafe fn CryptoGetTextPassword2(&self, defined: *mut i32, password: *mut *mut u16) -> HRESULT;
}

#[interface("23170F69-40C1-278A-0000-000600030000")]
pub unsafe trait IArchiveSetProperties: IUnknown {
    pub unsafe fn SetProperties(&self, names: *const *const u16, values: *const PropVariant, count: u32) -> HRESULT;
}

#[interface("23170F69-40C1-278A-0000-000600600000")]
pub unsafe trait IInArchive: IUnknown {
    pub unsafe fn Open(&self, stream: *mut c_void, max_check_start: *const u64, callback: *mut c_void) -> HRESULT;
    pub unsafe fn Close(&self) -> HRESULT;
    pub unsafe fn GetNumberOfItems(&self, count: *mut u32) -> HRESULT;
    pub unsafe fn GetProperty(&self, index: u32, prop_id: u32, value: *mut PropVariant) -> HRESULT;
    pub unsafe fn Extract(&self, indices: *const u32, count: u32, test: i32, callback: *mut c_void) -> HRESULT;
    pub unsafe fn GetArchiveProperty(&self, prop_id: u32, value: *mut PropVariant) -> HRESULT;
    pub unsafe fn GetNumberOfProperties(&self, count: *mut u32) -> HRESULT;
    pub unsafe fn GetPropertyInfo(&self, index: u32, name: *mut *mut u16, prop_id: *mut u32, var_type: *mut u16) -> HRESULT;
    pub unsafe fn GetNumberOfArchiveProperties(&self, count: *mut u32) -> HRESULT;
    pub unsafe fn GetArchivePropertyInfo(&self, index: u32, name: *mut *mut u16, prop_id: *mut u32, var_type: *mut u16) -> HRESULT;
}

pub mod prop {
    pub const PATH: u32 = 3;
    pub const NAME: u32 = 4;
    pub const IS_DIR: u32 = 6;
    pub const SIZE: u32 = 7;
    pub const SOLID: u32 = 13;
    pub const ENCRYPTED: u32 = 15;
    pub const SYM_LINK: u32 = 54;
    pub const IS_ALT_STREAM: u32 = 63;
    pub const ERROR_FLAGS: u32 = 71;
    pub const NT_REPARSE: u32 = 89;
    pub const HARD_LINK: u32 = 90;
    pub const COPY_LINK: u32 = 95;
}

pub mod ask {
    pub const EXTRACT: i32 = 0;
}

pub mod op_result {
    pub const OK: i32 = 0;
    pub const UNSUPPORTED_METHOD: i32 = 1;
    pub const DATA_ERROR: i32 = 2;
    pub const CRC_ERROR: i32 = 3;
    pub const WRONG_PASSWORD: i32 = 9;
}

mod handler_prop {
    pub const NAME: u32 = 0;
    pub const CLASS_ID: u32 = 1;
    pub const EXTENSION: u32 = 2;
    pub const SIGNATURE: u32 = 6;
    pub const MULTI_SIGNATURE: u32 = 7;
    pub const SIGNATURE_OFFSET: u32 = 8;
}

// ---------------------------------------------------------------------------
// Library
// ---------------------------------------------------------------------------

type CreateObjectFn = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;
type GetNumberOfFormatsFn = unsafe extern "system" fn(*mut u32) -> HRESULT;
type GetHandlerProperty2Fn = unsafe extern "system" fn(u32, u32, *mut PropVariant) -> HRESULT;

#[derive(Clone, Debug)]
pub struct FormatInfo {
    pub name: String,
    pub class_id: GUID,
    pub extensions: Vec<String>,
    pub signatures: Vec<Vec<u8>>,
    pub signature_offset: u64,
}

pub struct SevenZip {
    module: *mut c_void,
    create_object: CreateObjectFn,
    pub formats: Vec<FormatInfo>,
}

impl Drop for SevenZip {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::FreeLibrary(self.module);
        }
    }
}

impl SevenZip {
    pub fn load(dll: &str) -> Option<Self> {
        use windows_sys::Win32::System::LibraryLoader::*;
        let module = unsafe {
            LoadLibraryExW(
                crate::win::wide(dll).as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if module.is_null() {
            return None;
        }
        let proc = |name: &[u8]| unsafe { GetProcAddress(module, name.as_ptr()) };
        let (Some(create), Some(count), Some(property)) =
            (proc(b"CreateObject\0"), proc(b"GetNumberOfFormats\0"), proc(b"GetHandlerProperty2\0"))
        else {
            unsafe { windows_sys::Win32::Foundation::FreeLibrary(module) };
            return None;
        };
        let mut lib = SevenZip {
            module,
            create_object: unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, CreateObjectFn>(create) },
            formats: Vec::new(),
        };
        let count = unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, GetNumberOfFormatsFn>(count) };
        let property = unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, GetHandlerProperty2Fn>(property) };

        let mut number = 0u32;
        if unsafe { count(&mut number) }.is_err() {
            return None;
        }
        for index in 0..number {
            let get = |id: u32| {
                let mut value = PropVariant::default();
                let hr = unsafe { property(index, id, &mut value) };
                hr.is_ok().then_some(value)
            };
            let name = get(handler_prop::NAME).and_then(|v| v.as_string()).unwrap_or_default();
            let class_id = get(handler_prop::CLASS_ID)
                .and_then(|v| v.as_bytes())
                .filter(|b| b.len() == 16)
                .map(|b| GUID::from_u128(u128::from_be_bytes(guid_bytes_to_be(&b))))
                .unwrap_or_else(GUID::zeroed);
            let extensions = get(handler_prop::EXTENSION)
                .and_then(|v| v.as_string())
                .map(|s| s.split(' ').filter(|e| !e.is_empty()).map(|e| e.to_lowercase()).collect())
                .unwrap_or_default();
            let mut signatures = Vec::new();
            if let Some(sig) = get(handler_prop::SIGNATURE).and_then(|v| v.as_bytes()) {
                if !sig.is_empty() {
                    signatures.push(sig);
                }
            }
            if let Some(multi) = get(handler_prop::MULTI_SIGNATURE).and_then(|v| v.as_bytes()) {
                let mut position = 0;
                while position < multi.len() {
                    let n = multi[position] as usize;
                    position += 1;
                    if n == 0 || n > multi.len() - position {
                        return None;
                    }
                    signatures.push(multi[position..position + n].to_vec());
                    position += n;
                }
            }
            if name.is_empty() || class_id == GUID::zeroed() {
                return None;
            }
            let signature_offset = get(handler_prop::SIGNATURE_OFFSET).and_then(|v| v.as_u64()).unwrap_or(0);
            lib.formats.push(FormatInfo { name, class_id, extensions, signatures, signature_offset });
        }
        (!lib.formats.is_empty()).then_some(lib)
    }

    pub fn create_reader(&self, class_id: &GUID) -> Option<IInArchive> {
        let mut raw = std::ptr::null_mut();
        let hr = unsafe { (self.create_object)(class_id, &IInArchive::IID, &mut raw) };
        (hr.is_ok() && !raw.is_null()).then(|| unsafe { IInArchive::from_raw(raw) })
    }

    fn by_name(&self, name: &str) -> Option<&FormatInfo> {
        self.formats.iter().find(|f| f.name.eq_ignore_ascii_case(name))
    }

    /// Try handlers in priority order: explicit hints, then (without hints) handlers
    /// registered for the extension, then signatures found in the first 64 KiB, then
    /// a fixed list of common formats. With `require_hint` only hints are tried.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        &self,
        stream: &IInStream,
        ext_hint: &str,
        callback: &IArchiveOpenCallback,
        code_page: u32,
        hints: &[String],
        require_hint: bool,
        stop: &dyn Fn() -> bool,
    ) -> Option<(IInArchive, String)> {
        let mut tried: Vec<GUID> = Vec::new();
        let mut stopped = false;
        let mut try_format = |format: &FormatInfo, stopped: &mut bool| -> Option<(IInArchive, String)> {
            if *stopped || stop() || tried.contains(&format.class_id) {
                return None;
            }
            tried.push(format.class_id);
            if unsafe { stream.Seek(0, 0, std::ptr::null_mut()) } != S_OK {
                *stopped = true;
                return None;
            }
            let reader = self.create_reader(&format.class_id)?;
            if let Ok(props) = reader.cast::<IArchiveSetProperties>() {
                let names = [crate::win::wide("cp")];
                let name_ptrs = [names[0].as_ptr()];
                let mut value = PropVariant::default();
                value.set_u32(code_page);
                unsafe {
                    let _ = props.SetProperties(name_ptrs.as_ptr(), &value, 1);
                }
            }
            let max_start: u64 = 1 << 20;
            let hr = unsafe { reader.Open(stream.as_raw(), &max_start, callback.as_raw()) };
            if hr != S_OK {
                if hr == E_ABORT || stop() {
                    *stopped = true;
                }
                return None;
            }
            Some((reader, format.name.clone()))
        };

        for hint in hints {
            if let Some(format) = self.by_name(hint) {
                if let Some(found) = try_format(format, &mut stopped) {
                    return Some(found);
                }
            }
        }
        if require_hint || stopped {
            return None;
        }
        if hints.is_empty() {
            let ext = ext_hint.trim_start_matches('.').to_lowercase();
            for format in self.formats.iter().filter(|f| f.extensions.contains(&ext)) {
                if !format.name.eq_ignore_ascii_case("brotli") {
                    if let Some(found) = try_format(format, &mut stopped) {
                        return Some(found);
                    }
                }
            }
            if stopped {
                return None;
            }
        }

        // Signature scan. Skippable LZ4/Zstandard frames are stepped over first.
        let mut prefix = vec![0u8; 65536];
        let mut count = 0u32;
        if unsafe { stream.Seek(0, 0, std::ptr::null_mut()) } != S_OK
            || unsafe { stream.Read(prefix.as_mut_ptr() as *mut c_void, prefix.len() as u32, &mut count) } != S_OK
        {
            return None;
        }
        prefix.truncate(count as usize);
        let mut frame = 0usize;
        while prefix.len() - frame >= 4
            && prefix[frame] & 0xF0 == 0x50
            && prefix[frame + 1] == 0x2A
            && prefix[frame + 2] == 0x4D
            && prefix[frame + 3] == 0x18
        {
            if prefix.len() - frame < 8 {
                return None;
            }
            let size = u32::from_le_bytes(prefix[frame + 4..frame + 8].try_into().unwrap()) as usize;
            if size > prefix.len() - frame - 8 {
                return None;
            }
            frame += 8 + size;
        }
        if frame != 0 && prefix.len() - frame < 4 {
            return None;
        }
        for format in &self.formats {
            if format.signature_offset > (prefix.len() - frame) as u64 {
                continue;
            }
            if frame != 0 && !["zstd", "lz4", "lz5", "lizard"].iter().any(|n| format.name.eq_ignore_ascii_case(n)) {
                continue;
            }
            let offset = frame + format.signature_offset as usize;
            for signature in &format.signatures {
                if signature.len() <= prefix.len() - offset && prefix[offset..offset + signature.len()] == signature[..] {
                    if let Some(found) = try_format(format, &mut stopped) {
                        return Some(found);
                    }
                }
            }
        }
        if frame != 0 || stopped {
            return None;
        }
        for name in ["7z", "zip", "Rar5", "Rar", "tar", "gzip", "bzip2", "xz", "wim"] {
            if let Some(format) = self.by_name(name) {
                if let Some(found) = try_format(format, &mut stopped) {
                    return Some(found);
                }
            }
        }
        None
    }
}

/// 7-Zip exposes class ids as the raw in-memory `GUID` (little-endian fields).
fn guid_bytes_to_be(raw: &[u8]) -> [u8; 16] {
    let mut be = [0u8; 16];
    be[0..4].copy_from_slice(&[raw[3], raw[2], raw[1], raw[0]]);
    be[4..6].copy_from_slice(&[raw[5], raw[4]]);
    be[6..8].copy_from_slice(&[raw[7], raw[6]]);
    be[8..16].copy_from_slice(&raw[8..16]);
    be
}

/// Whether the archive reports fatal errors (a prefix may still be readable).
pub fn has_archive_errors(archive: &IInArchive) -> bool {
    let mut flags = PropVariant::default();
    let hr = unsafe { archive.GetArchiveProperty(prop::ERROR_FLAGS, &mut flags) };
    hr.is_err() || (flags.vt != VT_EMPTY && (flags.vt != VT_UI4 || flags.data[0] as u32 != 0))
}

pub fn item_property(archive: &IInArchive, index: u32, id: u32) -> PropVariant {
    let mut value = PropVariant::default();
    unsafe {
        let _ = archive.GetProperty(index, id, &mut value);
    }
    value
}
