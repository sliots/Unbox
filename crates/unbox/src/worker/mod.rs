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
use std::sync::Arc;
use unbox_core::budget::{default_budget, parse_budget, OutputBudget};
use unbox_core::catalog::{classify, default_stream_name, ArchiveKind};
use unbox_core::decode::{candidate_code_pages, code_page_label, decode_one, utf8_validity};
use unbox_core::ipc::*;
use unbox_core::manifest::{Manifest, ManifestItem};
use unbox_core::raw_names::{looks_like_tar, looks_like_zip, read_raw_names};
use unbox_core::sanitize::{has_illegal_chars, is_mac_auxiliary, is_safe_relative_syntax, normalize_separators, sanitize_relative_path};
use windows_core::Interface;

const OUTER_TMP: &str = ".unbox_outer_tmp";

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

fn expand_inner(
    lib: &SevenZip,
    staging: &str,
    payload: &str,
    cp: u32,
    require_tar: bool,
    strict: bool,
    explicit_budget: u64,
) -> InnerResult {
    let payload_path = Path::new(payload);
    let is_tar = looks_like_tar(payload_path);
    let is_zip = looks_like_zip(payload_path);
    let ext = payload_path.extension().map(|e| format!(".{}", e.to_string_lossy().to_lowercase())).unwrap_or_default();

    if require_tar && !is_tar {
        return InnerResult::new(InnerKind::Failed, "inner_not_tar");
    }
    if !is_tar && !is_zip && ![".7z", ".rar", ".zip", ".tar", ".iso"].contains(&ext.as_str()) {
        return InnerResult::new(InnerKind::NotApplicable, "");
    }
    let Some(stream) = InFileStream::open(payload, false) else {
        return InnerResult::new(InnerKind::Failed, "cannot_open_inner_stream");
    };
    let password = (!require_tar).then(|| Arc::new(ArchivePassword::default()));
    let (open_cb, _) = OpenCallback::create(payload, password.clone());
    let hints: Vec<String> = if require_tar { vec!["tar".into()] } else { Vec::new() };
    let stop = || control().canceled() || password.as_ref().is_some_and(|p| p.requested());
    let Some((archive, _)) = lib.open(&stream, &ext, &open_cb, cp, &hints, require_tar, &stop) else {
        let kind = if control().canceled() { InnerKind::Canceled } else { InnerKind::Failed };
        return InnerResult::new(kind, "unrecognized_or_corrupted_inner_archive");
    };
    let mut count = 0u32;
    if has_archive_errors(&archive) || unsafe { archive.GetNumberOfItems(&mut count) }.is_err() {
        unsafe {
            let _ = archive.Close();
        }
        return InnerResult::new(InnerKind::Failed, "corrupted_inner_archive");
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
        items.iter().map(|i| ManifestItem { path: i.rel_disk_path.clone(), is_dir: i.is_dir, size: i.size, crc: i.crc }).collect(),
    );
    let Ok(mut file) = std::fs::OpenOptions::new().write(true).create_new(true).open(path) else { return false };
    serde_json::to_writer(&mut file, &manifest).is_ok() && file.flush().is_ok() && file.sync_all().is_ok()
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

    let Some(stream) = InFileStream::open(&args.archive, false) else {
        error(format!("Cannot open archive file: {}", args.archive), None);
        return 3;
    };
    let password = Arc::new(ArchivePassword::default());
    let (open_cb, open_state) = OpenCallback::create(&args.archive, Some(password.clone()));

    let leaf = Path::new(&args.archive).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let route = classify(&leaf);
    let compound_tar = route.kind == ArchiveKind::TarWrapper;
    let split = route.kind == ArchiveKind::Split;
    let ext = Path::new(&args.archive).extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let open_cp = if args.code_page != 0 { args.code_page } else { default_archive_code_page() };
    let stop = || control().canceled() || password.requested();
    let Some((archive, handler)) = lib.open(&stream, &ext, &open_cb, open_cp, &route.handler_hints, compound_tar || split, &stop) else {
        if control().canceled() {
            control().send(&WorkerEvent::Canceled);
            return 0;
        }
        if password.requested() {
            error("Password incorrect or data damaged", Some("password_or_data_error"));
        } else {
            error("Unsupported or unrecognized archive format", None);
        }
        return 4;
    };

    let close = |archive: &IInArchive| unsafe {
        let _ = archive.Close();
    };
    let mut count = 0u32;
    if has_archive_errors(&archive) || unsafe { archive.GetNumberOfItems(&mut count) }.is_err() {
        close(&archive);
        error("Archive is damaged or incomplete", None);
        return 5;
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
            let inner = expand_inner(&lib, &staging, &payload, effective_cp, compound_tar, cp_explicit, args.inner_budget);
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
