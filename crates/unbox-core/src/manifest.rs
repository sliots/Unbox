//! Delivery manifest written by the worker and the reconciliation rules used to
//! prove the final tree is exactly what the worker produced — no missing items, no
//! extra items, no size or digest mismatch.

use crate::fold;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const MAX_ITEMS: usize = 5_000_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestItem {
    /// Path relative to the delivery root, `\`-separated.
    #[serde(rename = "p")]
    pub path: String,
    #[serde(rename = "d")]
    pub is_dir: bool,
    #[serde(rename = "s")]
    pub size: u64,
    #[serde(rename = "c")]
    pub crc: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Manifest {
    pub version: u32,
    pub delivery_root: String,
    pub item_count: usize,
    pub items: Vec<ManifestItem>,
}

impl Manifest {
    pub fn new(delivery_root: String, items: Vec<ManifestItem>) -> Self {
        Self { version: 1, delivery_root, item_count: items.len(), items }
    }

    pub fn parse(json: &str) -> Result<Self, String> {
        let manifest: Manifest = serde_json::from_str(json).map_err(|e| format!("交付清单格式错误：{e}"))?;
        if manifest.version != 1 {
            return Err("交付清单版本不受支持".into());
        }
        if manifest.item_count != manifest.items.len() {
            return Err("交付清单条目数与 item_count 不一致".into());
        }
        Ok(manifest)
    }

    /// Re-root the manifest below a collapsed top-level directory.
    pub fn rebase(&self, prefix: &str) -> Manifest {
        let prefix = normalize_relative(prefix);
        let folded_prefix = fold(&prefix);
        let mut items = Vec::with_capacity(self.items.len());
        for item in &self.items {
            let mut path = normalize_relative(&item.path);
            let folded = fold(&path);
            if folded == folded_prefix {
                continue; // The collapsed root itself is no longer delivered content.
            }
            if folded.starts_with(&format!("{folded_prefix}\\")) {
                path = path.chars().skip(prefix.chars().count() + 1).collect();
            }
            items.push(ManifestItem { path, ..item.clone() });
        }
        Manifest::new(self.delivery_root.clone(), items)
    }
}

/// Normalise separators and trailing separators only — never strip valid characters
/// such as leading or trailing spaces.
pub fn normalize_relative(relative: &str) -> String {
    let norm = relative.replace('/', "\\");
    norm.trim_end_matches('\\').to_string()
}

pub fn is_unsafe_relative(normalized: &str) -> bool {
    if normalized.starts_with('\\') || normalized.contains(':') {
        return true;
    }
    normalized.split('\\').any(|segment| segment.is_empty() || segment == "." || segment == "..")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyMode {
    /// Paths, types and lengths.
    Structure,
    /// Additionally the CRC32 of every file (gate before recycling the source).
    StructureAndDigest,
}

#[derive(Debug, Default)]
pub struct VerifyReport {
    pub problems: Vec<String>,
    pub files: usize,
    pub directories: usize,
    pub total_bytes: u64,
    pub digests_checked: usize,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }

    pub fn describe(&self) -> String {
        if self.ok() {
            return format!("已逐项核对 {} 个文件、{} 个目录，共 {} 字节", self.files, self.directories, self.total_bytes);
        }
        let mut text = self.problems.iter().take(5).cloned().collect::<Vec<_>>().join("；");
        if self.problems.len() > 5 {
            text.push_str(&format!("（共 {} 项问题）", self.problems.len()));
        }
        text
    }
}

/// Expected tree built from a manifest. A platform walker reports what it finds on
/// disk through [`ExpectedTree::directory`] and [`ExpectedTree::file`], then
/// [`ExpectedTree::finish`] reports what is missing.
pub struct ExpectedTree<'a> {
    manifest: &'a Manifest,
    expected: HashMap<String, &'a ManifestItem>,
    expected_dirs: HashSet<String>,
    seen_files: HashSet<String>,
    seen_dirs: HashSet<String>,
    pub report: VerifyReport,
}

impl<'a> ExpectedTree<'a> {
    pub fn new(manifest: &'a Manifest) -> Self {
        let mut tree = ExpectedTree {
            manifest,
            expected: HashMap::new(),
            expected_dirs: HashSet::new(),
            seen_files: HashSet::new(),
            seen_dirs: HashSet::new(),
            report: VerifyReport::default(),
        };
        if manifest.items.len() > MAX_ITEMS {
            tree.report.problems.push(format!("交付条目数量超出上限（{}）", manifest.items.len()));
            return tree;
        }
        for item in &manifest.items {
            let normalized = normalize_relative(&item.path);
            if normalized.is_empty() || is_unsafe_relative(&normalized) {
                tree.report.problems.push(format!("交付清单包含非法路径：{}", item.path));
                continue;
            }
            let key = fold(&normalized);
            if let Some(existing) = tree.expected.get(&key) {
                // The same directory may be declared twice by the archive; merge those.
                // File duplicates, case collisions and type conflicts stay errors.
                if existing.is_dir && item.is_dir && existing.path == item.path {
                    continue;
                }
                tree.report.problems.push(format!("交付清单包含重复路径：{normalized}"));
                continue;
            }
            tree.expected.insert(key.clone(), item);
            let mut parent = key.as_str();
            while let Some(slash) = parent.rfind('\\') {
                parent = &parent[..slash];
                if parent.is_empty() {
                    break;
                }
                tree.expected_dirs.insert(parent.to_string());
            }
            if item.is_dir {
                tree.expected_dirs.insert(key);
            }
        }
        tree
    }

    /// `false` when the manifest itself is already invalid.
    pub fn usable(&self) -> bool {
        self.report.ok()
    }

    pub fn directory(&mut self, rel: &str) {
        let key = fold(rel);
        if !self.expected_dirs.contains(&key) {
            self.report.problems.push(format!("交付树包含清单外的目录：{rel}"));
        }
        self.seen_dirs.insert(key);
    }

    /// Report a file. `crc` is evaluated only in digest mode.
    pub fn file(&mut self, rel: &str, length: u64, mode: VerifyMode, crc: impl FnOnce() -> Result<u32, String>) {
        let key = fold(rel);
        let Some(item) = self.expected.get(&key).copied().filter(|i| !i.is_dir) else {
            self.report.problems.push(format!("交付树包含清单外的文件：{rel}"));
            return;
        };
        if length != item.size {
            self.report.problems.push(format!("文件长度不一致：{rel}（期望 {}，实际 {length}）", item.size));
            return;
        }
        if mode == VerifyMode::StructureAndDigest {
            match crc() {
                Ok(actual) if actual == item.crc => self.report.digests_checked += 1,
                Ok(actual) => {
                    self.report.problems.push(format!("文件摘要不一致：{rel}（期望 {:08X}，实际 {actual:08X}）", item.crc));
                    return;
                }
                Err(e) => {
                    self.report.problems.push(format!("无法读取文件做摘要校验：{rel}（{e}）"));
                    return;
                }
            }
        }
        self.seen_files.insert(key);
        self.report.total_bytes += item.size;
    }

    pub fn problem(&mut self, message: String) {
        self.report.problems.push(message);
    }

    pub fn finish(mut self) -> VerifyReport {
        for item in &self.manifest.items {
            let normalized = normalize_relative(&item.path);
            let key = fold(&normalized);
            if item.is_dir {
                if !self.seen_dirs.contains(&key) {
                    self.report.problems.push(format!("缺少应保留的目录：{normalized}"));
                }
            } else if !self.seen_files.contains(&key) {
                self.report.problems.push(format!("缺少应交付的文件：{normalized}"));
            }
        }
        self.report.files = self.seen_files.len();
        self.report.directories = self.seen_dirs.len();
        self.report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, is_dir: bool, size: u64) -> ManifestItem {
        ManifestItem { path: path.into(), is_dir, size, crc: 7 }
    }

    #[test]
    fn parse_checks_count() {
        let mut m = Manifest::new(String::new(), vec![item("a", false, 1)]);
        let json = serde_json::to_string(&m).unwrap();
        assert!(Manifest::parse(&json).is_ok());
        m.item_count = 3;
        assert!(Manifest::parse(&serde_json::to_string(&m).unwrap()).is_err());
    }

    #[test]
    fn rebase_drops_root() {
        let m = Manifest::new(String::new(), vec![item("Top", true, 0), item("Top\\a.txt", false, 1), item("top/b", true, 0)]);
        let r = m.rebase("Top");
        let paths: Vec<_> = r.items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, vec!["a.txt", "b"]);
    }

    #[test]
    fn verify_detects_extra_and_missing() {
        let m = Manifest::new(String::new(), vec![item("d\\a.txt", false, 3), item("b.txt", false, 1)]);
        let mut t = ExpectedTree::new(&m);
        assert!(t.usable());
        t.directory("d");
        t.file("D\\A.TXT", 3, VerifyMode::Structure, || Ok(0));
        t.file("extra", 1, VerifyMode::Structure, || Ok(0));
        let report = t.finish();
        assert_eq!(report.problems.len(), 2);
        assert_eq!(report.files, 1);
    }

    #[test]
    fn verify_digest() {
        let m = Manifest::new(String::new(), vec![item("a", false, 1)]);
        let mut t = ExpectedTree::new(&m);
        t.file("a", 1, VerifyMode::StructureAndDigest, || Ok(8));
        assert!(!t.finish().ok());
    }

    #[test]
    fn duplicate_directories_merge() {
        let m = Manifest::new(String::new(), vec![item("d", true, 0), item("d", true, 0), item("x", false, 0), item("X", false, 0)]);
        let t = ExpectedTree::new(&m);
        assert_eq!(t.report.problems.len(), 1);
    }

    #[test]
    fn unsafe_paths() {
        assert!(is_unsafe_relative("..\\a"));
        assert!(is_unsafe_relative("c:x"));
        assert!(is_unsafe_relative("a\\\\b"));
        assert!(!is_unsafe_relative(" leading.txt"));
    }
}
