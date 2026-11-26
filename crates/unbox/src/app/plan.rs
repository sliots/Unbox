//! Extraction plan: where the archive is, where its contents will land, and the
//! staging directory on the same volume.

use crate::win::{full_path_name, resolve_network_drive};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use unbox_core::naming::{archive_base_name, unique_target_directory};

#[derive(Clone, Debug)]
pub struct ExtractionPlan {
    pub source: String,
    pub parent: String,
    pub target_name: String,
    pub final_dir: String,
    pub staging: String,
    /// Spread the content into `parent` instead of keeping the target folder.
    pub here: bool,
}

pub fn random_token() -> String {
    let mut out = String::new();
    for _ in 0..2 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        hasher.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
}

pub fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

pub fn unique_target(parent: &str, base: &str) -> String {
    unique_target_directory(Path::new(parent), base, exists).to_string_lossy().into_owned()
}

/// A later volume of a set (`a.7z.003`, `a.part2.rar`, `a.z01`, `a.r00`) stands for the
/// whole set: extraction starts from the volume that opens it.
pub fn first_volume(archive: &Path) -> PathBuf {
    let (Some(dir), Some(name)) = (archive.parent(), archive.file_name()) else { return archive.to_path_buf() };
    unbox_core::naming::first_volume_name(&name.to_string_lossy(), |n| dir.join(n).is_file())
        .map(|first| dir.join(first))
        .unwrap_or_else(|| archive.to_path_buf())
}

impl ExtractionPlan {
    /// Plan for `archive`, extracted into `to` (default: its own folder); `here` spreads
    /// the content into that folder instead of a folder named after the archive.
    pub fn create(archive: &Path, to: Option<&Path>, here: bool) -> Self {
        let full = full_path_name(archive.as_os_str()).unwrap_or_else(|| archive.to_string_lossy().into_owned());
        // The worker and the verifier compare against handle final paths, which use UNC
        // names for mapped network drives.
        let source = resolve_network_drive(&full);
        let path = PathBuf::from(&source);
        // Use the physical spelling of folders (case, 8.3 names, mapped drives) so they
        // match the final paths that file handles report later.
        let physical = |dir: String| std::fs::canonicalize(&dir).map(|p| crate::win::fs_strip(&p)).unwrap_or(dir);
        let source_dir = physical(
            path.parent()
                .map(|p| p.to_string_lossy().into_owned())
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default().to_string_lossy().into_owned()),
        );
        let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let source = Path::new(&source_dir).join(&file_name).to_string_lossy().into_owned();
        let parent = match to {
            Some(dir) => {
                physical(resolve_network_drive(&full_path_name(dir.as_os_str()).unwrap_or_else(|| dir.to_string_lossy().into_owned())))
            }
            None => source_dir,
        };
        let final_dir = unique_target(&parent, &archive_base_name(&file_name));
        let target_name = Path::new(&final_dir).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let staging = Path::new(&parent).join(format!(".unzip_staging_{}", random_token())).to_string_lossy().into_owned();
        ExtractionPlan { source, parent, target_name, final_dir, staging, here }
    }

    pub fn source_name(&self) -> String {
        Path::new(&self.source).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    pub fn cleanup_staging(&self) {
        let _ = std::fs::remove_dir_all(&self.staging);
    }
}
