//! Delivery: reconcile the staging tree with the manifest, collapse a redundant
//! same-name root, propagate the Mark of the Web, commit with one directory rename,
//! verify again, and only then (optionally) recycle the source volumes.

use super::plan::{exists, unique_target, ExtractionPlan};
use super::recycle::{recycle_volumes, RecycleResult};
use crate::win::fs::{attributes, final_path, open_pinned};
use crate::win::{full_path_name, Handle};
use std::fs::File;
use std::io::Read;
use std::mem::ManuallyDrop;
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::FromRawHandle;
use unbox_core::ipc::VolumeIdentity;
use unbox_core::manifest::{normalize_relative, ExpectedTree, Manifest, VerifyMode, VerifyReport};
use unbox_core::naming::should_collapse;
use unbox_core::{eq_ignore_case, starts_with_ignore_case};

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

pub struct CompletionRequest {
    pub plan: ExtractionPlan,
    pub delivery_root: String,
    pub manifest: Manifest,
    pub recycle: bool,
    pub volumes: Vec<VolumeIdentity>,
}

#[derive(Default)]
pub struct CompletionResult {
    pub delivered: bool,
    pub final_dir: String,
    pub failure: String,
    pub summary: String,
    pub zone_marked: usize,
    pub kept_staging: String,
    pub recycle: Option<RecycleResult>,
}

fn trim(path: &str) -> String {
    path.trim_end_matches('\\').to_string()
}

fn resolve_delivery_root(staging: &str, relative: &str) -> Option<String> {
    let staging = trim(&full_path_name(staging.as_ref())?);
    let combined = if relative.trim().is_empty() {
        staging.clone()
    } else {
        let clean = normalize_relative(relative);
        if clean.is_empty() || clean.contains(':') || clean.contains("..") || clean.starts_with('\\') {
            return None;
        }
        trim(&full_path_name(format!("{staging}\\{clean}").as_ref())?)
    };
    (eq_ignore_case(&combined, &staging) || starts_with_ignore_case(&combined, &format!("{staging}\\"))).then_some(combined)
}

fn file_from_handle(handle: &Handle) -> ManuallyDrop<File> {
    ManuallyDrop::new(unsafe { File::from_raw_handle(handle.raw() as _) })
}

fn crc_of(handle: &Handle) -> Result<u32, String> {
    let mut file = file_from_handle(handle);
    let mut hasher = crc32fast::Hasher::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => return Ok(hasher.finalize()),
            Ok(n) => hasher.update(&buffer[..n]),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Walk a tree from a root whose final path was already checked. Every entry is
/// opened, pinned and its final path compared with the expected physical path, so a
/// junction or a swapped ancestor is reported instead of followed. Parent handles stay
/// open until all their children are processed.
fn walk_verified_tree(
    root: Handle,
    root_physical: &str,
    allow_write: bool,
    on_directory: &mut dyn FnMut(&str),
    on_file: &mut dyn FnMut(&str, &str, &Handle),
    on_problem: &mut dyn FnMut(String),
) {
    enum Frame {
        Enter(String, Handle),
        Exit(#[allow(dead_code)] Handle),
    }
    let root_physical = trim(root_physical);
    let rel_of = |full: &str| -> String {
        let prefix = format!("{root_physical}\\");
        if starts_with_ignore_case(full, &prefix) && full.is_char_boundary(prefix.len()) {
            full[prefix.len()..].to_string()
        } else {
            std::path::Path::new(full).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        }
    };
    let mut stack = vec![Frame::Enter(root_physical.clone(), root)];
    while let Some(frame) = stack.pop() {
        let Frame::Enter(dir, handle) = frame else { continue };
        stack.push(Frame::Exit(handle));
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                on_problem(format!("无法枚举目录 {}：{e}", rel_of(&dir)));
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                on_problem(format!("无法枚举目录 {}", rel_of(&dir)));
                continue;
            };
            let full = format!("{dir}\\{}", entry.file_name().to_string_lossy());
            let rel = rel_of(&full);
            let Some(entry_handle) = open_pinned(&full, allow_write) else {
                on_problem(format!("无法打开条目 {rel}"));
                continue;
            };
            let attrs = attributes(entry_handle.raw());
            let final_ok = final_path(entry_handle.raw()).is_some_and(|f| eq_ignore_case(&f, &full));
            match attrs {
                Some(a) if final_ok && a & FILE_ATTRIBUTE_REPARSE_POINT == 0 => {
                    if a & FILE_ATTRIBUTE_DIRECTORY != 0 {
                        on_directory(&rel);
                        stack.push(Frame::Enter(full, entry_handle));
                    } else {
                        on_file(&full, &rel, &entry_handle);
                    }
                }
                _ => on_problem(format!("交付树包含非预期的联接点/符号链接：{rel}")),
            }
        }
    }
}

/// Open `root` and require its final path to equal its lexical full path.
fn open_root(root: &str, allow_write: bool) -> Result<(Handle, String), String> {
    let expected = trim(&full_path_name(root.as_ref()).ok_or("无法解析交付根目录")?);
    let handle = open_pinned(root, allow_write).ok_or("无法打开交付根目录（可能已被替换为联接点/符号链接）")?;
    match final_path(handle.raw()) {
        Some(f) if eq_ignore_case(&f, &expected) => Ok((handle, f)),
        _ => Err("交付根目录或其祖先包含联接点/符号链接".into()),
    }
}

pub fn verify(root: &str, manifest: &Manifest, mode: VerifyMode) -> VerifyReport {
    if !std::path::Path::new(root).is_dir() {
        return VerifyReport { problems: vec!["交付根目录不存在".into()], ..Default::default() };
    }
    let mut tree = ExpectedTree::new(manifest);
    if !tree.usable() {
        return tree.finish();
    }
    let (handle, physical) = match open_root(root, true) {
        Ok(opened) => opened,
        Err(message) => {
            tree.problem(message);
            return tree.finish();
        }
    };
    let tree = std::cell::RefCell::new(tree);
    walk_verified_tree(
        handle,
        &physical,
        mode != VerifyMode::StructureAndDigest,
        &mut |rel| tree.borrow_mut().directory(rel),
        &mut |_, rel, h| {
            let length = file_from_handle(h).metadata().map(|m| m.len()).unwrap_or(u64::MAX);
            tree.borrow_mut().file(rel, length, mode, || crc_of(h));
        },
        &mut |message| tree.borrow_mut().problem(message),
    );
    tree.into_inner().finish()
}

/// Copy the source's `Zone.Identifier` stream onto every delivered file while the
/// tree is still private.
fn propagate_zone(source: &str, root: &str) -> usize {
    let Ok(zone) = std::fs::read(format!("{source}:Zone.Identifier")) else { return 0 };
    if zone.is_empty() {
        return 0;
    }
    let Ok((handle, physical)) = open_root(root, true) else { return 0 };
    let mut marked = 0;
    walk_verified_tree(
        handle,
        &physical,
        true,
        &mut |_| {},
        &mut |full, _, _| {
            // The file and its ancestors are still pinned while the stream is written.
            if std::fs::write(format!("{full}:Zone.Identifier"), &zone).is_ok() {
                marked += 1;
            }
        },
        &mut |_| {},
    );
    marked
}

fn is_truly_empty(plan: &ExtractionPlan, delivery_root: &str) -> bool {
    resolve_delivery_root(&plan.staging, delivery_root)
        .and_then(|root| std::fs::read_dir(root).ok())
        .is_some_and(|mut entries| entries.next().is_none())
        && !exists(plan.final_dir.as_ref())
}

fn collapse_candidate(root: &str, archive: &str) -> Option<String> {
    let entries: Vec<(String, bool)> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| {
            let plain_dir = e.metadata().is_ok_and(|m| {
                let a = m.file_attributes();
                a & FILE_ATTRIBUTE_DIRECTORY != 0 && a & FILE_ATTRIBUTE_REPARSE_POINT == 0
            });
            (e.file_name().to_string_lossy().into_owned(), plain_dir)
        })
        .collect();
    let file_name = std::path::Path::new(archive).file_name()?.to_string_lossy().into_owned();
    should_collapse(&entries, &file_name)
}

pub fn complete(mut request: CompletionRequest) -> CompletionResult {
    let mut result = CompletionResult::default();
    let plan = &mut request.plan;
    if !std::path::Path::new(&plan.staging).is_dir() {
        result.failure = "暂存目录不存在，可能已被清理".into();
        return result;
    }
    if request.manifest.items.is_empty() && !is_truly_empty(plan, &request.delivery_root) {
        result.failure = "交付清单为空，无法确认任何产物".into();
        return result;
    }
    let Some(mut delivery_root) = resolve_delivery_root(&plan.staging, &request.delivery_root).filter(|r| std::path::Path::new(r).is_dir())
    else {
        result.failure = "交付根目录不存在".into();
        result.kept_staging = plan.staging.clone();
        return result;
    };

    let pre = verify(&delivery_root, &request.manifest, VerifyMode::Structure);
    if !pre.ok() {
        result.failure = format!("交付内容与清单不一致：{}", pre.describe());
        result.kept_staging = plan.staging.clone();
        return result;
    }

    let mut prefix = None;
    if let Some(child) = collapse_candidate(&delivery_root, &plan.source) {
        delivery_root = format!("{delivery_root}\\{child}");
        prefix = Some(child);
    }

    // The target may have appeared since planning; never touch an existing directory.
    if exists(plan.final_dir.as_ref()) {
        plan.final_dir = unique_target(&plan.parent, &plan.target_name);
        plan.target_name = std::path::Path::new(&plan.final_dir).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    }

    result.zone_marked = propagate_zone(&plan.source, &delivery_root);

    // One same-volume rename: there is no half-committed state.
    if let Err(e) = std::fs::rename(&delivery_root, &plan.final_dir) {
        result.failure = format!("提交目录失败：{e}");
        result.kept_staging = plan.staging.clone();
        return result;
    }
    let staging_full = full_path_name(plan.staging.as_ref()).map(|p| trim(&p)).unwrap_or_default();
    let mut leftover = String::new();
    if !eq_ignore_case(&trim(&delivery_root), &staging_full) {
        if let Err(e) = std::fs::remove_dir_all(&plan.staging) {
            leftover = e.to_string();
        }
    }

    let final_manifest = match &prefix {
        Some(p) => request.manifest.rebase(p),
        None => request.manifest.clone(),
    };
    let post = verify(&plan.final_dir, &final_manifest, VerifyMode::Structure);
    result.final_dir = plan.final_dir.clone();
    if !post.ok() {
        result.failure = format!("提交后复核失败：{}", post.describe());
        return result;
    }
    result.delivered = true;
    result.summary = if leftover.is_empty() {
        format!("已交付 {} 个文件、{} 个目录", post.files, post.directories)
    } else {
        format!("已交付 {} 个文件；暂存目录清理延迟：{leftover}", post.files)
    };

    if request.recycle && !request.volumes.is_empty() {
        let digest = verify(&plan.final_dir, &final_manifest, VerifyMode::StructureAndDigest);
        result.recycle = Some(if digest.ok() {
            recycle_volumes(&request.volumes)
        } else {
            RecycleResult::failed(format!("回收前摘要复核未通过，源包已保留：{}", digest.describe()))
        });
    }
    result
}
