//! Target naming: archive base names, collision-free target directories, the
//! single-same-name-root collapse rule and volume family detection.

use crate::catalog::{self, ArchiveKind};
use crate::eq_ignore_case;
use std::path::{Path, PathBuf};

fn strip_suffix_ignore_ascii_case<'a>(value: &'a str, suffix: &str) -> Option<&'a str> {
    crate::ends_with_ignore_ascii_case(value, suffix).then(|| &value[..value.len() - suffix.len()])
}

/// Archive name without archive and volume suffixes (`a.tar.gz` → `a`,
/// `a.part01.rar` → `a`, `a.7z.001` → `a`, `a.zip` → `a`).
pub fn archive_base_name(file_name: &str) -> String {
    if let Some(format) = catalog::match_name(file_name).filter(|f| f.kind == ArchiveKind::TarWrapper) {
        return file_name[..file_name.len() - format.suffix.len()].to_string();
    }

    // ^(.*?)\.part\d+\.rar$
    if let Some(stem) = strip_suffix_ignore_ascii_case(file_name, ".rar") {
        let lower = stem.to_ascii_lowercase();
        if let Some(pos) = lower.rfind(".part") {
            let digits = &stem[pos + 5..];
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                return stem[..pos].to_string();
            }
        }
    }

    // ^(.*?)(?:\.(?:7z|zip))?\.001$
    if let Some(stem) = strip_suffix_ignore_ascii_case(file_name, ".001") {
        for inner in [".7z", ".zip"] {
            if let Some(base) = strip_suffix_ignore_ascii_case(stem, inner) {
                return base.to_string();
            }
        }
        return stem.to_string();
    }

    match file_name.rfind('.') {
        Some(dot) => file_name[..dot].to_string(),
        None => file_name.to_string(),
    }
}

/// First of `parent\name.ext`, `parent\name (2).ext`, `parent\name (3).ext` … that does
/// not exist.
pub fn unique_target_file(parent: &Path, file_name: &str, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let (stem, ext) = match file_name.rfind('.') {
        Some(dot) if dot > 0 => (&file_name[..dot], &file_name[dot..]),
        _ => (file_name, ""),
    };
    let mut candidate = parent.join(file_name);
    let mut n = 2;
    while exists(&candidate) {
        candidate = parent.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    candidate
}

/// First of `parent\base`, `parent\base (2)`, `parent\base (3)` … that does not exist.
pub fn unique_target_directory(parent: &Path, base: &str, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let candidate = parent.join(base);
    if !exists(&candidate) {
        return candidate;
    }
    (2u64..).map(|index| parent.join(format!("{base} ({index})"))).find(|path| !exists(path)).expect("an unused name exists")
}

/// Collapse rule: the delivery root holds exactly one entry, it is an ordinary
/// directory, and its name equals the archive's base name.
pub fn should_collapse(entries: &[(String, bool)], archive_file_name: &str) -> Option<String> {
    let [(name, is_plain_dir)] = entries else { return None };
    if !*is_plain_dir {
        return None;
    }
    let base = archive_base_name(archive_file_name);
    (!base.is_empty() && eq_ignore_case(name, &base)).then(|| name.clone())
}

/// Whether `other` belongs to the same multi-volume set as `first` (same directory
/// and `.001/.002`, `.zip/.z01` or `.partN.rar` naming).
pub fn is_same_volume_family(first: &Path, other: &Path) -> bool {
    let parent_eq = match (first.parent(), other.parent()) {
        (Some(a), Some(b)) => eq_ignore_case(&a.to_string_lossy(), &b.to_string_lossy()),
        (None, None) => true,
        _ => false,
    };
    if !parent_eq {
        return false;
    }
    let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let (fn1, fn2) = (name(first), name(other));
    if fn1 == fn2 {
        return true;
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());

    if let (Some(d1), Some(d2)) = (fn1.rfind('.'), fn2.rfind('.')) {
        let (base1, ext1) = (&fn1[..d1], &fn1[d1 + 1..]);
        let (base2, ext2) = (&fn2[..d2], &fn2[d2 + 1..]);
        if base1 == base2 {
            if digits(ext1) && digits(ext2) {
                return true;
            }
            if ext1 == "zip" && ext2.len() >= 2 && ext2.starts_with('z') && ext2[1..].bytes().all(|b| b.is_ascii_digit()) {
                return true;
            }
        }
    }

    if let (Some(p1), Some(p2)) = (fn1.rfind(".part"), fn2.rfind(".part")) {
        if p1 == p2 {
            let (rest1, rest2) = (&fn1[p1 + 5..], &fn2[p2 + 5..]);
            if rest1.len() > 4 && rest2.len() > 4 && rest1.is_char_boundary(rest1.len() - 4) && rest2.is_char_boundary(rest2.len() - 4) {
                let (num1, ext1) = rest1.split_at(rest1.len() - 4);
                let (num2, ext2) = rest2.split_at(rest2.len() - 4);
                if ext1 == ".rar" && ext2 == ".rar" && digits(num1) && digits(num2) && fn1[..p1] == fn2[..p2] {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {

    #[test]
    fn unique_file_names() {
        let taken = ["/r/a.7z", "/r/a (2).7z", "/r/noext"];
        let exists = |p: &Path| taken.iter().any(|t| Path::new(t) == p);
        assert_eq!(unique_target_file(Path::new("/r"), "a.7z", exists), PathBuf::from("/r/a (3).7z"));
        assert_eq!(unique_target_file(Path::new("/r"), "noext", exists), PathBuf::from("/r/noext (2)"));
        assert_eq!(unique_target_file(Path::new("/r"), ".hidden", exists), PathBuf::from("/r/.hidden"));
    }

    use super::*;

    #[test]
    fn base_names() {
        assert_eq!(archive_base_name("测试.zip"), "测试");
        assert_eq!(archive_base_name("a.tar.gz"), "a");
        assert_eq!(archive_base_name("a.TGZ"), "a");
        assert_eq!(archive_base_name("movie.part01.rar"), "movie");
        assert_eq!(archive_base_name("x.part1.part2.rar"), "x.part1");
        assert_eq!(archive_base_name("disk.7z.001"), "disk");
        assert_eq!(archive_base_name("disk.001"), "disk");
        assert_eq!(archive_base_name("noext"), "noext");
        assert_eq!(archive_base_name("包 (2).7z"), "包 (2)");
    }

    #[test]
    fn unique_names() {
        let taken = [PathBuf::from("/p/a"), PathBuf::from("/p/a (2)")];
        let path = unique_target_directory(Path::new("/p"), "a", |p| taken.iter().any(|t| t == p));
        assert_eq!(path, PathBuf::from("/p/a (3)"));
    }

    #[test]
    fn collapse() {
        assert_eq!(should_collapse(&[("希捷专修".into(), true)], "希捷专修.zip"), Some("希捷专修".into()));
        assert_eq!(should_collapse(&[("x".into(), true)], "希捷专修 (2).zip"), None);
        assert_eq!(should_collapse(&[("a".into(), false)], "a.zip"), None);
        assert_eq!(should_collapse(&[("a".into(), true), ("b".into(), true)], "a.zip"), None);
    }

    #[test]
    fn volume_family() {
        let p = |s: &str| PathBuf::from(s);
        assert!(is_same_volume_family(&p("/d/a.7z.001"), &p("/d/a.7z.002")));
        assert!(is_same_volume_family(&p("/d/a.zip"), &p("/d/a.z01")));
        assert!(is_same_volume_family(&p("/d/a.part1.rar"), &p("/d/a.part2.rar")));
        assert!(!is_same_volume_family(&p("/d/a.part1.rar"), &p("/e/a.part2.rar")));
        assert!(!is_same_volume_family(&p("/d/a.rar"), &p("/d/b.rar")));
    }
}
