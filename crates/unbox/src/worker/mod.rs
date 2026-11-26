//! The extraction worker: a child process that hosts 7z.dll, writes into the staging
//! directory, and reports progress and a delivery manifest to the UI process.

mod callbacks;
mod control;
mod sevenzip;

use crate::win::fs::{ensure_directory, resolve_canonical_directory, PinnedDirectories};
use crate::win::{default_archive_code_page, disk_free, WinCodePages};
use callbacks::*;
use control::{control, ArchivePassword};
use sevenzip::*;
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex};
use unbox_core::budget::{default_budget, parse_budget, OutputBudget};
use unbox_core::catalog::{classify, default_stream_name, ArchiveKind};
use unbox_core::decode::{candidate_code_pages, code_page_label, decode_one, utf8_validity};
use unbox_core::ipc::*;
use unbox_core::manifest::{Manifest, ManifestItem};
use unbox_core::raw_names::{looks_like_tar, looks_like_zip, read_raw_names, sniff_archive};
use unbox_core::sanitize::{has_illegal_chars, is_mac_auxiliary, is_safe_relative_syntax, normalize_separators, sanitize_relative_path};
use windows_core::Interface;

const OUTER_TMP: &str = ".unbox_outer_tmp";
/// How many archive-in-archive layers are unpacked after the outer one.
const MAX_PEELED_LAYERS: u32 = 4;

/// Guesses for an inner archive: the outer password first, it is the likeliest.
fn password_guesses(outer: &ArchivePassword, guesses: &[String]) -> Vec<String> {
    let mut all: Vec<String> = outer.current_text().into_iter().collect();
    for g in guesses {
        if !all.contains(g) {
            all.push(g.clone());
        }
    }
    all
}

#[derive(Default)]
struct Args {
    archive: String,
    staging: String,
    dll: String,
    manifest: String,
    code_page: u32,
    budget: u64,
    inner_budget: u64,
    managed: bool,
}

fn error(message: impl Into<String>, reason: Option<&str>) {
    control().send(&WorkerEvent::Error { message: message.into(), error_reason: reason.map(str::to_string) });
}

fn parse_args(args: &[OsString]) -> Result<Args, ()> {
    let mut out = Args::default();
    let mut iter = args.iter().map(|a| a.to_string_lossy().into_owned());
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--archive" => out.archive = iter.next().unwrap_or_default(),
            "--staging" => out.staging = iter.next().unwrap_or_default(),
            "--dll" => out.dll = iter.next().unwrap_or_default(),
            "--manifest" => out.manifest = iter.next().unwrap_or_default(),
            "--cp" => out.code_page = iter.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            "--managed" => out.managed = true,
            "--max-output-bytes" | "--max-inner-output-bytes" => {
                let value = iter.next().and_then(|v| parse_budget(&v)).ok_or(())?;
                if arg == "--max-output-bytes" {
                    out.budget = value;
                } else {
                    out.inner_budget = value;
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn has_disk_space(dir: &str, minimum: u64) -> bool {
    minimum == 0 || disk_free(Path::new(dir)).is_some_and(|free| free >= minimum)
}

fn is_disk_full(hr: windows::core::HRESULT, write_error: u32) -> bool {
    const FULL: [u32; 2] = [112, 39]; // ERROR_DISK_FULL, ERROR_HANDLE_DISK_FULL
    FULL.contains(&write_error) || FULL.iter().any(|c| hr == hr_from_win32(*c))
}

// ---------------------------------------------------------------------------
// Item plan: decoded names, filters and skip decisions
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ItemPlan {
    target_paths: Vec<String>,
    to_extract: Vec<bool>,
    illegal: Vec<u32>,
    policy_filtered: u32,
    declared_unicode: u32,
    legacy_names: u32,
    undecodable: u32,
    all_sizes_known: bool,
    declared_bytes: u64,
    unsafe_items: u32,
}

fn build_item_plan(archive: &IInArchive, count: u32, archive_path: &str, cp: u32, is_tar: bool, strict: bool) -> ItemPlan {
    let mut plan = ItemPlan {
        target_paths: vec![String::new(); count as usize],
        to_extract: vec![false; count as usize],
        all_sizes_known: true,
        ..Default::default()
    };
    // Raw name bytes take precedence over the engine's decoding; a count mismatch
    // means the indexes cannot be trusted, so the raw names are dropped.
    let raw = read_raw_names(Path::new(archive_path)).filter(|names| names.len() == count as usize);

    for index in 0..count {
        let i = index as usize;
        let engine_name = item_property(archive, index, prop::PATH).as_string().unwrap_or_default();
        let mut path = match raw.as_ref().map(|r| &r[i]).filter(|r| !r.raw.is_empty()) {
            Some(entry) => {
                let decoded = decode_one(&WinCodePages, &entry.raw, entry.declared_utf8, cp, strict);
                if entry.declared_utf8 {
                    plan.declared_unicode += 1;
                } else if utf8_validity(&entry.raw).1 {
                    plan.legacy_names += 1;
                }
                if decoded.ambiguous || decoded.undecodable {
                    plan.undecodable += 1;
                }
                decoded.name
            }
            None => engine_name,
        };
        if path.is_empty() && count == 1 {
            let leaf = Path::new(archive_path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            path = default_stream_name(&classify(&leaf), &leaf);
        }

        let present = |id| item_property(archive, index, id).is_present();
        if !is_safe_relative_syntax(&path)
            || present(prop::SYM_LINK)
            || present(prop::NT_REPARSE)
            || present(prop::HARD_LINK)
            || present(prop::COPY_LINK)
            || present(prop::IS_ALT_STREAM)
        {
            plan.unsafe_items += 1;
            continue;
        }
        path = normalize_separators(&path);

        let size = item_property(archive, index, prop::SIZE).as_u64();
        let is_dir = item_property(archive, index, prop::IS_DIR).as_bool();

        if is_tar {
            path = path.split('\\').filter(|s| !s.is_empty() && *s != ".").collect::<Vec<_>>().join("\\");
            if path.is_empty() {
                if !is_dir {
                    plan.unsafe_items += 1;
                }
                continue; // An explicit "./" directory is the existing root.
            }
        }

        if is_mac_auxiliary(&path) {
            plan.policy_filtered += 1;
            plan.target_paths[i] = path;
            continue;
        }
        if has_illegal_chars(&path) {
            plan.illegal.push(index);
        }
        plan.target_paths[i] = path;
        plan.to_extract[i] = true;
        if !is_dir {
            match size {
                Some(size) if size <= u64::MAX - plan.declared_bytes => plan.declared_bytes += size,
                _ => plan.all_sizes_known = false,
            }
        }
    }
    plan
}

fn encoding_options(preferred: u32, archive_path: &str) -> Vec<EncodingOption> {
    let raw = read_raw_names(Path::new(archive_path)).unwrap_or_default();
    candidate_code_pages(preferred)
        .into_iter()
        .map(|cp| {
            let samples = raw
                .iter()
                .filter(|e| !e.declared_utf8 && !e.raw.is_empty())
                .filter(|e| matches!(utf8_validity(&e.raw), (true, true)))
                .map(|e| unbox_core::decode::CodePages::decode(&WinCodePages, &e.raw, cp).0)
                .filter(|s| !s.is_empty())
                .take(3)
                .collect();
            EncodingOption { cp, label: code_page_label(cp), samples }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Passwords
// ---------------------------------------------------------------------------

enum OpenFailure {
    Canceled,
    Unrecognized,
}

struct Opened {
    archive: IInArchive,
    handler: String,
    callback: IArchiveOpenCallback,
    state: Arc<Mutex<OpenState>>,
}

/// Open the archive. When opening needs a password (encrypted headers), a wrong guess
/// or answer makes the open fail; the next guess is tried, then the user is asked again
/// until the archive opens or the task is canceled.
#[allow(clippy::too_many_arguments)]
fn open_with_password(
    lib: &SevenZip,
    stream: &IInStream,
    path: &str,
    ext: &str,
    code_page: u32,
    hints: &[String],
    require_hint: bool,
    password: Option<&Arc<ArchivePassword>>,
) -> Result<Opened, OpenFailure> {
    loop {
        if let Some(p) = password {
            p.begin_attempt();
        }
        let (callback, state) = OpenCallback::create(path, password.cloned());
        // Once a handler asks for a password the format is known: no other handler is tried.
        let stop = || control().canceled() || password.is_some_and(|p| p.asked());
        if let Some((archive, handler)) = lib.open(stream, ext, &callback, code_page, hints, require_hint, &stop) {
            return Ok(Opened { archive, handler, callback, state });
        }
        if control().canceled() {
            return Err(OpenFailure::Canceled);
        }
        match password {
            Some(p) if p.asked() => p.reject(),
            _ => return Err(OpenFailure::Unrecognized),
        }
    }
}

/// Settle the password before extraction by decoding one encrypted item: guesses are
/// tried silently, then the user is asked until a password decodes it. Returns `false`
/// when canceled.
fn verify_password(archive: &IInArchive, count: u32, password: &ArchivePassword) -> bool {
    if password.has_current() {
        return true; // The encrypted headers already proved it.
    }
    // The cheapest item to decode: the first one of a solid stream, else the smallest.
    let mut solid = PropVariant::default();
    unsafe {
        let _ = archive.GetArchiveProperty(prop::SOLID, &mut solid);
    }
    let solid = solid.as_bool();
    let mut best: Option<(u64, u32)> = None;
    for index in 0..count {
        if !item_property(archive, index, prop::ENCRYPTED).as_bool() || item_property(archive, index, prop::IS_DIR).as_bool() {
            continue;
        }
        let size = item_property(archive, index, prop::SIZE).as_u64();
        if size == Some(0) {
            continue;
        }
        let key = if solid { index as u64 } else { size.unwrap_or(u64::MAX) };
        if best.is_none_or(|(k, _)| key < k) {
            best = Some((key, index));
        }
    }
    let Some((_, index)) = best else {
        // Nothing to verify against: a password, if any, comes from the user only.
        password.drop_guesses();
        return true;
    };
    loop {
        let Some(candidate) = password.get() else { return false };
        match probe_password(archive, index, candidate) {
            Probe::Correct | Probe::Unknown => return true,
            Probe::Wrong if control().canceled() => return false,
            Probe::Wrong => password.reject(),
        }
    }
}

// ---------------------------------------------------------------------------
// Inner expansion (compound TAR / split payloads)
// ---------------------------------------------------------------------------

#[derive(PartialEq, Eq)]
enum InnerKind {
    NotApplicable,
    Success,
    Failed,
    Canceled,
}

struct InnerResult {
    kind: InnerKind,
    delivery_root: String,
    reason: String,
    items: Vec<DeliveredItem>,
    limit: u64,
    written: u64,
    size_known: bool,
    total_items: u32,
    policy_filtered: u32,
    unsafe_items: u32,
    missing: u32,
    completed: u64,
}

impl InnerResult {
    fn new(kind: InnerKind, reason: &str) -> Self {
        InnerResult {
            kind,
            delivery_root: String::new(),
            reason: reason.into(),
            items: Vec::new(),
            limit: 0,
            written: 0,
            size_known: true,
            total_items: 0,
            policy_filtered: 0,
            unsafe_items: 0,
            missing: 0,
            completed: 0,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn expand_inner(
    lib: &SevenZip,
    staging: &str,
    payload: &str,
    cp: u32,
    require_tar: bool,
    strict: bool,
    explicit_budget: u64,
    guesses: Vec<String>,
) -> InnerResult {
    let payload_path = Path::new(payload);
    let is_tar = looks_like_tar(payload_path);
    let is_zip = looks_like_zip(payload_path);
    let ext = payload_path.extension().map(|e| format!(".{}", e.to_string_lossy().to_lowercase())).unwrap_or_default();

    if require_tar && !is_tar {
        return InnerResult::new(InnerKind::Failed, "inner_not_tar");
    }
    let sniffed = sniff_archive(payload_path);
    if !is_tar && !is_zip && sniffed.is_none() && ![".7z", ".rar", ".zip", ".tar", ".iso"].contains(&ext.as_str()) {
        return InnerResult::new(InnerKind::NotApplicable, "");
    }
    let Some(stream) = InFileStream::open(payload, false) else {
        return InnerResult::new(InnerKind::Failed, "cannot_open_inner_stream");
    };
    let password = (!require_tar).then(|| Arc::new(ArchivePassword::with_guesses(guesses)));
    let hints: Vec<String> = if require_tar { vec!["tar".into()] } else { sniffed.map(|h| vec![h.to_string()]).unwrap_or_default() };
    let opened = open_with_password(lib, &stream, payload, &ext, cp, &hints, require_tar, password.as_ref());
    let (archive, open_cb) = match opened {
        Ok(opened) => (opened.archive, opened.callback),
        Err(OpenFailure::Canceled) => return InnerResult::new(InnerKind::Canceled, ""),
        Err(OpenFailure::Unrecognized) => return InnerResult::new(InnerKind::Failed, "unrecognized_or_corrupted_inner_archive"),
    };
    let mut count = 0u32;
    if has_archive_errors(&archive) || unsafe { archive.GetNumberOfItems(&mut count) }.is_err() {
        unsafe {
            let _ = archive.Close();
        }
        return InnerResult::new(InnerKind::Failed, "corrupted_inner_archive");
    }
    if let Some(password) = &password {
        if !verify_password(&archive, count, password) {
            unsafe {
                let _ = archive.Close();
            }
            return InnerResult::new(InnerKind::Canceled, "");
        }
    }

    // Isolated delivery root committed only after the inner archive fully succeeds.
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let deliver_name = format!(".unbox_deliver_{}_{stamp}", std::process::id());
    let deliver_dir = format!("{}\\{deliver_name}", staging.trim_end_matches('\\'));
    let close = |archive: &IInArchive| unsafe {
        let _ = archive.Close();
    };
    if std::fs::create_dir(&deliver_dir).is_err() {
        close(&archive);
        return InnerResult::new(InnerKind::Failed, "cannot_create_inner_delivery_root");
    }
    let Some(canonical) = resolve_canonical_directory(&deliver_dir) else {
        let _ = std::fs::remove_dir_all(&deliver_dir);
        close(&archive);
        return InnerResult::new(InnerKind::Failed, "cannot_resolve_inner_delivery_root");
    };

    let mut plan = build_item_plan(&archive, count, payload, cp, is_tar, strict);
    // Inner illegal names are batch-renamed without asking again.
    for &index in &plan.illegal {
        plan.target_paths[index as usize] = sanitize_relative_path(&plan.target_paths[index as usize]);
    }
    let limit = if explicit_budget != 0 { explicit_budget } else { default_budget(plan.declared_bytes, 128 << 20) };
    if !has_disk_space(&deliver_dir, plan.declared_bytes.max(explicit_budget)) {
        close(&archive);
        let _ = std::fs::remove_dir_all(&deliver_dir);
        return InnerResult::new(InnerKind::Failed, "insufficient_disk_space");
    }
    let budget = Arc::new(OutputBudget::new(limit));
    let (callback, shared) = ExtractCallback::create(ExtractConfig {
        archive: archive.clone(),
        extraction_root: deliver_dir.clone(),
        canonical_root: canonical,
        target_paths: plan.target_paths.clone(),
        to_extract: plan.to_extract.clone(),
        total_bytes: plan.declared_bytes,
        progress: Arc::new(Progress::new(Phase::Inner, plan.all_sizes_known)),
        password,
        budget: budget.clone(),
    });
    let hr = unsafe { archive.Extract(std::ptr::null(), u32::MAX, 0, callback.as_raw()) };
    let outcome = shared.finish();
    drop(callback);
    close(&archive);
    drop(archive);
    drop(open_cb);
    drop(stream);

    let reason = if budget.exceeded() {
        "output_budget_exceeded"
    } else if is_disk_full(hr, outcome.write_error) {
        "insufficient_disk_space"
    } else if outcome.wrong_password {
        "wrong_password"
    } else if outcome.crc_error {
        "crc_error"
    } else if outcome.data_error {
        "data_error"
    } else if outcome.unsupported {
        "unsupported_method"
    } else if outcome.write_error != 0 || outcome.has_error {
        "extract_error"
    } else if hr != S_OK {
        "corrupted_inner_archive"
    } else {
        ""
    };

    let mut result = InnerResult::new(InnerKind::Success, reason);
    result.total_items = count;
    result.policy_filtered = plan.policy_filtered;
    result.unsafe_items = plan.unsafe_items + outcome.unsafe_count;
    result.limit = budget.limit();
    result.written = budget.used();
    result.size_known = plan.all_sizes_known;
    result.missing = outcome.missing;
    result.completed = outcome.completed_bytes;

    if control().canceled() {
        let _ = std::fs::remove_dir_all(&deliver_dir);
        result.kind = InnerKind::Canceled;
    } else if !reason.is_empty() {
        let _ = std::fs::remove_dir_all(&deliver_dir);
        result.kind = InnerKind::Failed;
    } else {
        result.items = outcome.delivered;
        result.delivery_root = deliver_name;
    }
    result
}

fn write_manifest(path: &str, delivery_root: &str, items: &[DeliveredItem]) -> bool {
    use std::io::Write;
    if path.is_empty() {
        return true;
    }
    let manifest = Manifest::new(
        delivery_root.to_string(),
        items
            .iter()
            .map(|i| ManifestItem { path: i.rel_disk_path.clone(), is_dir: i.is_dir, size: i.size, crc: i.crc, peeled: i.peeled })
            .collect(),
    );
    let Ok(mut file) = std::fs::OpenOptions::new().write(true).create_new(true).open(path) else { return false };
    serde_json::to_writer(&mut file, &manifest).is_ok() && file.flush().is_ok() && file.sync_all().is_ok()
}

/// The volumes of a numbered split set (`a.7z.001` …) whose first volume holds an
/// archive, with that archive's handler. `Ok(None)`: a plain split file, joined to disk
/// by the Split handler. `Err`: the set is visibly incomplete.
fn split_volumes(first: &str, leaf: &str) -> Result<Option<(Vec<String>, &'static str)>, String> {
    let dir = match first.rfind(['\\', '/']) {
        Some(slash) => &first[..=slash],
        None => "",
    };
    let siblings: Vec<String> = std::fs::read_dir(if dir.is_empty() { "." } else { dir })
        .map(|entries| entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    let stem = &leaf[..leaf.len().saturating_sub(4)];
    let names = match unbox_core::naming::numbered_volumes(leaf, &siblings) {
        None => return Ok(None),
        Some(Err(n)) => {
            return Err(format!("缺少分卷「{stem}.{n:03}」。请把全部分卷放在同一文件夹后重试，源压缩包已保留。"));
        }
        Some(Ok(names)) => names,
    };
    let Some(hint @ ("7z" | "Rar5" | "Rar" | "zip")) = sniff_archive(Path::new(first)) else { return Ok(None) };
    let paths: Vec<String> = names.iter().map(|n| format!("{dir}{n}")).collect();
    if hint == "7z" {
        let mut head = [0u8; 32];
        let declared = std::fs::File::open(first)
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head))
            .ok()
            .and_then(|_| unbox_core::raw_names::sevenzip_declared_size(&head));
        let total: u64 = paths.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
        if declared.is_some_and(|size| size > total) {
            let next = format!("{stem}.{:03}", names.len() + 1);
            return Err(format!("分卷不完整：缺少「{next}」及之后的分卷。请把全部分卷放在同一文件夹后重试，源压缩包已保留。"));
        }
    }
    Ok(Some((paths, hint)))
}

// ---------------------------------------------------------------------------
// Main flow
// ---------------------------------------------------------------------------

pub fn run(raw_args: &[OsString]) -> i32 {
    let Ok(args) = parse_args(raw_args) else {
        error("Invalid output budget", None);
        return 1;
    };
    if args.archive.is_empty() || args.staging.is_empty() {
        error("Missing required arguments", None);
        return 1;
    }
    if args.managed && !control::wait_for_start() {
        return 1;
    }
    control().listen(args.managed);
    let _com = ComApartment::init();

    let Some(staging_root) = resolve_canonical_directory(&args.staging) else {
        error("Cannot verify staging directory", None);
        return 7;
    };
    let mut pinned_staging = PinnedDirectories::default();
    if ensure_directory(&staging_root, &staging_root, &mut pinned_staging).is_err() {
        error("Cannot verify staging directory", None);
        return 7;
    }

    let dll = if args.dll.is_empty() {
        std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("7z.dll"))).unwrap_or_default().to_string_lossy().into_owned()
    } else {
        std::path::absolute(&args.dll).map(|p| p.to_string_lossy().into_owned()).unwrap_or(args.dll.clone())
    };
    let Some(lib) = SevenZip::load(&dll) else {
        error(format!("Failed to load 7z.dll at: {dll}"), None);
        return 2;
    };

    let guesses = unbox_core::password::guesses(Path::new(&args.archive));
    let password = Arc::new(ArchivePassword::with_guesses(guesses.clone()));

    let leaf = Path::new(&args.archive).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let route = classify(&leaf);
    let compound_tar = route.kind == ArchiveKind::TarWrapper;
    let mut split = route.kind == ArchiveKind::Split;
    let mut ext = Path::new(&args.archive).extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let mut hints = route.handler_hints.clone();
    let mut volumes = Vec::new();
    if split {
        match split_volumes(&args.archive, &leaf) {
            Ok(Some((set, hint))) => {
                // An archive split into numbered volumes opens in place, read across the
                // volumes; only a plain split file is joined to disk.
                ext = Path::new(&leaf)
                    .file_stem()
                    .and_then(|s| Path::new(s).extension())
                    .map(|e| format!(".{}", e.to_string_lossy()))
                    .unwrap_or_default();
                hints = vec![hint.to_string()];
                volumes = set;
                split = false;
            }
            Ok(None) => {}
            Err(message) => {
                error(message, None);
                return 5;
            }
        }
    }
    let stream = if volumes.is_empty() {
        InFileStream::open(&args.archive, false)
    } else {
        MultiVolumeStream::open(&volumes).map(|(stream, _)| stream)
    };
    let Some(stream) = stream else {
        error(format!("Cannot open archive file: {}", args.archive), None);
        return 3;
    };
    let open_cp = if args.code_page != 0 { args.code_page } else { default_archive_code_page() };
    let opened = open_with_password(
        &lib,
        &stream,
        &args.archive,
        &ext,
        open_cp,
        &hints,
        compound_tar || split || !volumes.is_empty(),
        Some(&password),
    );
    let Opened { archive, handler, callback: open_cb, state: open_state } = match opened {
        Ok(opened) => opened,
        Err(OpenFailure::Canceled) => {
            control().send(&WorkerEvent::Canceled);
            return 0;
        }
        Err(OpenFailure::Unrecognized) => {
            error("Unsupported or unrecognized archive format", None);
            return 4;
        }
    };

    {
        let mut state = open_state.lock().unwrap();
        for volume in volumes.iter().skip(1) {
            state.record(volume);
        }
    }

    let close = |archive: &IInArchive| unsafe {
        let _ = archive.Close();
    };
    let mut count = 0u32;
    if has_archive_errors(&archive) || unsafe { archive.GetNumberOfItems(&mut count) }.is_err() {
        close(&archive);
        error("Archive is damaged or incomplete", None);
        return 5;
    }
    if !verify_password(&archive, count, &password) {
        close(&archive);
        control().send(&WorkerEvent::Canceled);
        return 0;
    }

    let mut effective_cp = open_cp;
    let mut cp_explicit = args.code_page != 0;
    let is_tar = handler.eq_ignore_ascii_case("tar");
    let mut plan = build_item_plan(&archive, count, &args.archive, effective_cp, is_tar, cp_explicit);

    // Contradictory declarations or undecodable names: let the user pick the encoding.
    if plan.undecodable > 0 || (plan.declared_unicode > 0 && plan.legacy_names > 0) {
        let event = WorkerEvent::EncodingOptions { options: encoding_options(effective_cp, &args.archive) };
        match control().ask_encoding(event) {
            Ok(chosen) => {
                // An explicit choice switches to strict decoding so the preview and the
                // written names always agree.
                if chosen != 0 {
                    effective_cp = chosen;
                    cp_explicit = true;
                    plan = build_item_plan(&archive, count, &args.archive, effective_cp, is_tar, true);
                }
            }
            Err(canceled) => {
                close(&archive);
                if canceled {
                    control().send(&WorkerEvent::Canceled);
                    return 0;
                }
                error("Encoding prompt timed out", Some("prompt_timeout"));
                return 5;
            }
        }
    }

    // Stored LZ4 frames have a provable size, which makes the budget exact.
    let mut stored_lz4 = None;
    if !plan.all_sizes_known && count == 1 && plan.to_extract[0] && handler.eq_ignore_ascii_case("lz4") {
        if let Ok(mut file) = std::fs::File::open(&args.archive) {
            let size = file.metadata().map(|m| m.len()).unwrap_or(0);
            stored_lz4 = unbox_core::lz4::scan_stored_frame(&mut file, size, &mut || control().check());
        }
        if control().canceled() {
            close(&archive);
            control().send(&WorkerEvent::Canceled);
            return 0;
        }
        if let Some(size) = stored_lz4 {
            plan.declared_bytes = size;
            plan.all_sizes_known = true;
        }
    }

    if !has_disk_space(&args.staging, plan.declared_bytes.max(args.budget)) {
        close(&archive);
        error("Insufficient disk space in staging directory", Some("insufficient_disk_space"));
        return 7;
    }

    let mut user_skipped = false;
    if !plan.illegal.is_empty() {
        let items = plan
            .illegal
            .iter()
            .map(|&i| InvalidItem {
                index: i,
                raw: plan.target_paths[i as usize].clone(),
                sanitized: sanitize_relative_path(&plan.target_paths[i as usize]),
            })
            .collect();
        match control().ask_invalid_chars(WorkerEvent::InvalidCharsFound { items }) {
            Ok(InvalidCharsAction::BatchRename) => {
                for &i in &plan.illegal {
                    plan.target_paths[i as usize] = sanitize_relative_path(&plan.target_paths[i as usize]);
                }
            }
            Ok(InvalidCharsAction::Skip) => {
                for &i in &plan.illegal {
                    plan.to_extract[i as usize] = false;
                }
                user_skipped = true;
            }
            Err(canceled) => {
                close(&archive);
                if canceled {
                    control().send(&WorkerEvent::Canceled);
                    return 0;
                }
                error("Filename prompt timed out", Some("prompt_timeout"));
                return 5;
            }
        }
    }

    // Compound TAR and split volumes go to an isolated directory first; the payload
    // decides whether an inner archive is expanded.
    let needs_inner = compound_tar || split;
    // Work below the verified physical path so lexical aliases (case, 8.3 names) can
    // never fail the handle-level containment checks.
    let staging = staging_root.trim_end_matches('\\').to_string();
    let outer_root = if needs_inner { format!("{staging}\\{OUTER_TMP}") } else { staging.clone() };
    if needs_inner && std::fs::create_dir(&outer_root).is_err() {
        close(&archive);
        error("Cannot create staging sub-directory", None);
        return 7;
    }
    let Some(canonical_root) = resolve_canonical_directory(&outer_root) else {
        close(&archive);
        error("Cannot resolve staging directory", None);
        return 7;
    };

    let budget_limit = match stored_lz4 {
        Some(size) if args.budget != 0 => args.budget.min(size),
        Some(size) => size,
        None if args.budget != 0 => args.budget,
        None => default_budget(plan.declared_bytes, 256 << 20),
    };
    let budget = Arc::new(OutputBudget::new(budget_limit));
    control().send(&WorkerEvent::Init {
        total_items: count,
        total_bytes: plan.declared_bytes,
        size_known: plan.all_sizes_known,
        policy_filtered_items: plan.policy_filtered,
        code_page: effective_cp,
        budget_bytes: budget_limit,
    });

    let (callback, shared) = ExtractCallback::create(ExtractConfig {
        archive: archive.clone(),
        extraction_root: outer_root.clone(),
        canonical_root,
        target_paths: plan.target_paths.clone(),
        to_extract: plan.to_extract.clone(),
        total_bytes: plan.declared_bytes,
        progress: Arc::new(Progress::new(Phase::Outer, plan.all_sizes_known)),
        password: Some(password.clone()),
        budget: budget.clone(),
    });
    let hr = unsafe { archive.Extract(std::ptr::null(), u32::MAX, 0, callback.as_raw()) };
    let outcome = shared.finish();

    // Release every COM object and file handle before touching the file system.
    drop(callback);
    close(&archive);
    drop(archive);
    let (opened_volumes, volume_identities) = {
        let state = open_state.lock().unwrap();
        (state.opened_volumes.clone(), state.identities.clone())
    };
    drop(open_cb);
    drop(stream);

    if control().canceled() {
        control().send(&WorkerEvent::Canceled);
        return 0;
    }

    let mut delivered = outcome.delivered;
    let mut completed = outcome.completed_bytes;
    let mut unsafe_items = plan.unsafe_items + outcome.unsafe_count;
    let mut missing = outcome.missing;
    let mut limit = budget.limit();
    let mut written = budget.used();
    let mut intermediate = 0u64;
    let mut size_known = plan.all_sizes_known;
    let mut phase = Phase::Outer;
    let pw = password.requested();
    let mut reason: String = if budget.exceeded() {
        "output_budget_exceeded"
    } else if is_disk_full(hr, outcome.write_error) {
        "insufficient_disk_space"
    } else if outcome.wrong_password {
        "wrong_password"
    } else if outcome.crc_error {
        if pw {
            "password_or_data_error"
        } else {
            "crc_error"
        }
    } else if outcome.data_error {
        if pw {
            "password_or_data_error"
        } else {
            "data_error"
        }
    } else if outcome.unsupported {
        "unsupported_method"
    } else if outcome.write_error != 0 || outcome.has_error {
        "extract_error"
    } else if hr != S_OK {
        if pw {
            "password_or_data_error"
        } else {
            "extract_error"
        }
    } else if stored_lz4.is_some_and(|size| written != size) {
        "data_error"
    } else {
        ""
    }
    .to_string();

    let mut hard_failure = outcome.has_error || budget.exceeded() || !reason.is_empty();
    let mut delivery_root = String::new();
    let mut policy_filtered = plan.policy_filtered;
    let mut total_items = count;

    if !hard_failure && needs_inner {
        if delivered.len() == 1 && !delivered[0].is_dir {
            let payload = delivered[0].disk_path.clone();
            // The outer password (if any) is the likeliest one for the inner archive.
            let inner_guesses = password_guesses(&password, &guesses);
            let inner = expand_inner(&lib, &staging, &payload, effective_cp, compound_tar, cp_explicit, args.inner_budget, inner_guesses);
            match inner.kind {
                InnerKind::Success => {
                    delivered = inner.items;
                    total_items = inner.total_items;
                    policy_filtered += inner.policy_filtered;
                    completed = inner.completed;
                    delivery_root = inner.delivery_root;
                    // Inner rejections and gaps must never be hidden by an outer success.
                    unsafe_items += inner.unsafe_items;
                    missing += inner.missing;
                    let _ = std::fs::remove_file(&payload);
                    let _ = std::fs::remove_dir_all(&outer_root);
                }
                InnerKind::Failed => {
                    hard_failure = true;
                    reason = if inner.reason.is_empty() { "corrupted_archive".into() } else { inner.reason };
                    phase = Phase::Inner;
                    limit = inner.limit;
                    written = inner.written;
                    intermediate = delivered[0].size;
                    size_known = inner.size_known;
                    delivered.clear();
                }
                InnerKind::Canceled => {
                    control().send(&WorkerEvent::Canceled);
                    return 0;
                }
                InnerKind::NotApplicable => delivery_root = OUTER_TMP.into(),
            }
        } else if compound_tar {
            hard_failure = true;
            reason = "inner_payload_missing".into();
            delivered.clear();
        } else {
            delivery_root = OUTER_TMP.into();
        }
    }

    // A lone delivered file that is an archive by content (whatever its extension) is
    // unpacked as well, layer after layer. Like the source archive, every unpacked inner
    // archive is kept (next to its content); the UI recycles it together with the source
    // when the user asked for that. If a layer cannot be unpacked, the file is delivered
    // as it is.
    let mut kept: Vec<DeliveredItem> = Vec::new();
    let mut layers = 0;
    while !hard_failure && layers < MAX_PEELED_LAYERS && delivered.len() == 1 && !delivered[0].is_dir {
        let item = &delivered[0];
        if item.rel_disk_path.contains(['\\', '/']) || sniff_archive(Path::new(&item.disk_path)).is_none() {
            break;
        }
        let payload = item.disk_path.clone();
        let inner_guesses = password_guesses(&password, &guesses);
        let inner = expand_inner(&lib, &staging, &payload, effective_cp, false, cp_explicit, args.inner_budget, inner_guesses);
        match inner.kind {
            InnerKind::Success if !inner.items.is_empty() => {
                // Move the archive just unpacked (and the ones kept before it) next to the
                // new content, then drop the previous layer's directory.
                let new_root = format!("{staging}\\{}", inner.delivery_root);
                let mut moved = Vec::new();
                for mut item in kept.drain(..).chain(std::iter::once(DeliveredItem { peeled: true, ..delivered[0].clone() })) {
                    let name = Path::new(&item.disk_path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let target = unbox_core::naming::unique_target_file(Path::new(&new_root), &name, |p| p.exists());
                    if std::fs::rename(&item.disk_path, &target).is_ok() {
                        item.rel_disk_path = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        item.disk_path = target.to_string_lossy().into_owned();
                        moved.push(item);
                    }
                }
                kept = moved;
                if !delivery_root.is_empty() {
                    let _ = std::fs::remove_dir_all(format!("{staging}\\{delivery_root}"));
                }
                delivered = inner.items;
                total_items = inner.total_items;
                policy_filtered += inner.policy_filtered;
                completed = inner.completed;
                delivery_root = inner.delivery_root;
                unsafe_items += inner.unsafe_items;
                missing += inner.missing;
                layers += 1;
            }
            InnerKind::Canceled => {
                control().send(&WorkerEvent::Canceled);
                return 0;
            }
            _ => break,
        }
    }

    delivered.extend(kept);

    if hard_failure {
        if needs_inner {
            let _ = std::fs::remove_dir_all(&outer_root);
        }
        delivered.clear();
    }

    let manifest_ok = !hard_failure && write_manifest(&args.manifest, &delivery_root, &delivered);
    if !hard_failure && !manifest_ok {
        hard_failure = true;
        reason = "manifest_write_failed".into();
    }
    drop(pinned_staging);

    let partial = user_skipped || unsafe_items > 0 || missing > 0;
    let status = if hard_failure {
        FinishStatus::Failed
    } else if partial {
        FinishStatus::Partial
    } else {
        FinishStatus::Ok
    };
    control().send(&WorkerEvent::Finished(Box::new(FinishedReport {
        status,
        total_items,
        extracted_items: delivered.len() as u32,
        skipped_items: if user_skipped { plan.illegal.len() as u32 } else { 0 },
        policy_filtered_items: policy_filtered,
        unsafe_items,
        missing_items: missing,
        completed_bytes: completed,
        empty_archive: delivered.is_empty() && !hard_failure,
        code_page: effective_cp,
        error_reason: reason,
        phase,
        limit_bytes: limit,
        written_bytes: written,
        intermediate_bytes: intermediate,
        size_known,
        delivery_root,
        manifest: args.manifest.clone(),
        manifest_ok,
        opened_volumes,
        volume_identities,
    })));
    status.exit_code()
}

/// COM must be initialised on the thread that talks to 7z.dll objects.
struct ComApartment;

impl ComApartment {
    fn init() -> Self {
        unsafe {
            windows_sys::Win32::System::Com::CoInitializeEx(std::ptr::null(), windows_sys::Win32::System::Com::COINIT_MULTITHREADED as u32);
        }
        ComApartment
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::System::Com::CoUninitialize() };
    }
}
