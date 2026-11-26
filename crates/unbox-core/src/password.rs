//! Password guesses tried before asking: shared archives often use their own name or
//! the folder they were published in as the password.

use crate::naming::archive_base_name;
use std::path::Path;

/// Upper bound on automatic guesses, so a deep path with many words cannot delay the
/// password prompt noticeably.
pub const MAX_GUESSES: usize = 40;

/// Candidate passwords for `archive`, nearest first: the archive name (without archive /
/// volume suffixes), then each enclosing folder up to the drive root (archives are often
/// nested two or three folders below the one named after the password). Every name is
/// followed by its whitespace-separated words. Duplicates and empty strings are dropped.
pub fn guesses(archive: &Path) -> Vec<String> {
    let base = archive.file_name().map(|n| archive_base_name(&n.to_string_lossy())).unwrap_or_default();
    let folders = archive.ancestors().skip(1).map_while(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());

    let mut out: Vec<String> = Vec::new();
    for name in std::iter::once(base).chain(folders) {
        let words: Vec<&str> = name.split(char::is_whitespace).filter(|w| !w.is_empty()).collect();
        let whole = name.trim();
        let parts = if words.len() > 1 { words } else { Vec::new() };
        for candidate in std::iter::once(whole).chain(parts) {
            if out.len() == MAX_GUESSES {
                return out;
            }
            if !candidate.is_empty() && !out.iter().any(|c| c == candidate) {
                out.push(candidate.to_string());
            }
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
        assert_eq!(guesses(path), ["合集 第一部", "合集", "第一部", "资源 2024 pw123", "资源", "2024", "pw123", "下载"]);
    }

    #[test]
    fn duplicates_and_full_width_space() {
        let path = Path::new("/data/secret/secret.7z.001");
        assert_eq!(guesses(path), ["secret", "data"]);
        let path = Path::new("/a\u{3000}b/x.zip");
        assert_eq!(guesses(path), ["x", "a\u{3000}b", "a", "b"]);
        let path = Path::new("/pw-top/mid/inner/x.zip");
        assert_eq!(guesses(path), ["x", "inner", "mid", "pw-top"]);
    }

    #[test]
    fn root_folder_has_no_name() {
        assert_eq!(guesses(Path::new("/x y.zip")), ["x y", "x", "y"]);
    }
}
