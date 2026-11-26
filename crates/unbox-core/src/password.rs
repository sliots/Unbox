//! Password guesses tried before asking: shared archives often use their own name or
//! the folder they were published in as the password.

use crate::naming::archive_base_name;
use std::path::Path;

/// Upper bound on automatic guesses, so a folder name with many words cannot delay
/// the password prompt noticeably.
pub const MAX_GUESSES: usize = 24;

/// Candidate passwords for `archive`, most likely first: the archive name (without
/// archive / volume suffixes), the parent folder name, then the words of each name
/// split on whitespace. Duplicates and empty strings are dropped.
pub fn guesses(archive: &Path) -> Vec<String> {
    let base = archive.file_name().map(|n| archive_base_name(&n.to_string_lossy())).unwrap_or_default();
    let folder = archive.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

    let mut out: Vec<String> = Vec::new();
    let mut push = |candidate: &str| {
        if !candidate.is_empty() && out.len() < MAX_GUESSES && !out.iter().any(|c| c == candidate) {
            out.push(candidate.to_string());
        }
    };
    for name in [&base, &folder] {
        push(name.trim());
    }
    for name in [&base, &folder] {
        let words: Vec<&str> = name.split(char::is_whitespace).filter(|w| !w.is_empty()).collect();
        if words.len() > 1 {
            words.iter().for_each(|w| push(w));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_then_words() {
        let path = Path::new("/下载/资源 2024 pw123/合集 第一部.part01.rar");
        assert_eq!(guesses(path), ["合集 第一部", "资源 2024 pw123", "合集", "第一部", "资源", "2024", "pw123"]);
    }

    #[test]
    fn duplicates_and_full_width_space() {
        let path = Path::new("/data/secret/secret.7z.001");
        assert_eq!(guesses(path), ["secret"]);
        let path = Path::new("/a\u{3000}b/x.zip");
        assert_eq!(guesses(path), ["x", "a\u{3000}b", "a", "b"]);
    }

    #[test]
    fn root_folder_has_no_name() {
        assert_eq!(guesses(Path::new("/x y.zip")), ["x y", "x", "y"]);
    }
}
