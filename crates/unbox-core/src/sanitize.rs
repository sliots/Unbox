//! Lexical path policy for archive entries: unsafe syntax, Windows-illegal names,
//! batch renaming and macOS auxiliary files.

const ILLEGAL: &[char] = &[':', '*', '?', '"', '<', '>', '|'];
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4",
    "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

fn upper(value: &str) -> String {
    value.to_uppercase()
}

fn base_before_dot(segment: &str) -> &str {
    segment.split('.').next().unwrap_or("")
}

/// macOS metadata that is filtered by product rule (`__MACOSX/`, `.DS_Store`, `._*`).
pub fn is_mac_auxiliary(rel: &str) -> bool {
    let norm = rel.replace('/', "\\");
    if norm == "__MACOSX" || norm.starts_with("__MACOSX\\") {
        return true;
    }
    let file = norm.rsplit('\\').next().unwrap_or("");
    file.eq_ignore_ascii_case(".DS_Store") || file.starts_with("._")
}

/// Whether one name segment has characters or a form Windows cannot store.
pub fn has_illegal_segment(segment: &str) -> bool {
    if segment.is_empty() {
        return false;
    }
    if segment.chars().any(|c| (c as u32) < 32 || ILLEGAL.contains(&c)) {
        return true;
    }
    if segment.ends_with(' ') || segment.ends_with('.') {
        return true;
    }
    RESERVED.contains(&upper(base_before_dot(segment)).as_str())
}

/// Whether a relative path needs renaming before it can be written on Windows.
pub fn has_illegal_chars(rel: &str) -> bool {
    let norm = rel.replace('/', "\\");
    if norm.contains(':') || norm.starts_with("\\\\") {
        return true;
    }
    norm.split('\\').any(|segment| segment == ".." || has_illegal_segment(segment))
}

pub fn normalize_separators(rel: &str) -> String {
    rel.replace('/', "\\")
}

/// Rejects absolute, drive-relative, traversing and device paths before any other
/// processing.
pub fn is_safe_relative_syntax(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.starts_with('\\') || path.contains(':') || path.contains('\0') {
        return false;
    }
    for raw in path.split(['\\', '/']) {
        let segment = raw.trim_end_matches(' ');
        if segment == ".." || (segment != "." && !segment.is_empty() && segment.chars().all(|c| c == '.')) {
            return false;
        }
        let device = upper(base_before_dot(segment).trim_end_matches(' '));
        if matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
            return false;
        }
        let chars: Vec<char> = device.chars().collect();
        if chars.len() == 4 && (device.starts_with("COM") || device.starts_with("LPT")) && "123456789¹²³".contains(chars[3]) {
            return false;
        }
    }
    true
}

/// Default batch rename: illegal characters and trailing dots/spaces become `-`,
/// reserved device names get a `_` prefix.
pub fn sanitize_segment(segment: &str) -> String {
    let mut chars: Vec<char> =
        segment.chars().map(|c| if (c as u32) < 32 || ILLEGAL.contains(&c) || c == '/' || c == '\\' { '-' } else { c }).collect();
    let mut end = chars.len();
    while end > 0 && matches!(chars[end - 1], ' ' | '.') {
        end -= 1;
    }
    for c in &mut chars[end..] {
        *c = '-';
    }
    let result: String = chars.into_iter().collect();
    if RESERVED.contains(&upper(base_before_dot(&result)).as_str()) {
        format!("_{result}")
    } else {
        result
    }
}

pub fn sanitize_relative_path(rel: &str) -> String {
    rel.replace('/', "\\")
        .split('\\')
        .filter(|segment| !segment.is_empty())
        .map(|segment| sanitize_segment(if segment == ".." { "__" } else { segment }))
        .collect::<Vec<_>>()
        .join("\\")
}

/// Remove the `\\?\` / `\\?\UNC\` prefix for comparisons.
pub fn strip_extended_prefix(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("\\\\?\\UNC\\") {
        return format!("\\\\{rest}");
    }
    path.strip_prefix("\\\\?\\").unwrap_or(path).to_string()
}

/// Whether `final_path` equals or lies below `root` (case-insensitive, separator
/// boundary aware).
pub fn is_under_root(root: &str, final_path: &str) -> bool {
    let root = root.trim_end_matches('\\');
    let path = final_path.trim_end_matches('\\');
    if root.is_empty() || path.is_empty() || path.len() < root.len() {
        return false;
    }
    if !crate::starts_with_ignore_case(path, root) {
        return false;
    }
    // Compare the boundary on the folded strings so multi-byte case pairs line up.
    let (fr, fp) = (crate::fold(root), crate::fold(path));
    fp.get(fr.len()..).is_some_and(|rest| rest.is_empty() || rest.starts_with('\\'))
}

/// Join a safe relative entry below `root`, dropping `.` segments. Returns `None` for
/// anything that would leave the root.
pub fn join_below(root: &str, rel: &str) -> Option<String> {
    if root.is_empty() || !is_safe_relative_syntax(rel) {
        return None;
    }
    let mut out = root.trim_end_matches('\\').to_string();
    let mut any = false;
    for segment in rel.split(['\\', '/']) {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." || segment.contains(':') {
            return None;
        }
        out.push('\\');
        out.push_str(segment);
        any = true;
    }
    any.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_files() {
        assert!(is_mac_auxiliary("__MACOSX/a/b"));
        assert!(is_mac_auxiliary("dir/.DS_Store"));
        assert!(is_mac_auxiliary("dir\\._x"));
        assert!(!is_mac_auxiliary("dir/_x"));
    }

    #[test]
    fn illegal() {
        assert!(has_illegal_chars("a/b?.txt"));
        assert!(has_illegal_chars("CON.txt"));
        assert!(has_illegal_chars("dir./a"));
        assert!(!has_illegal_chars("normal/name.txt"));
        assert_eq!(sanitize_relative_path("a:b/c*?.txt"), "a-b\\c--.txt");
        assert_eq!(sanitize_relative_path("nul.txt"), "_nul.txt");
        assert_eq!(sanitize_relative_path("end. "), "end--");
        assert_eq!(sanitize_relative_path("../x"), "__\\x");
    }

    #[test]
    fn safe_syntax() {
        assert!(is_safe_relative_syntax("a/./b"));
        assert!(!is_safe_relative_syntax("../a"));
        assert!(!is_safe_relative_syntax("/abs"));
        assert!(!is_safe_relative_syntax("c:x"));
        assert!(!is_safe_relative_syntax("a/.../b"));
        assert!(!is_safe_relative_syntax("dir/com¹.txt"));
        assert!(!is_safe_relative_syntax("CONOUT$"));
        assert!(is_safe_relative_syntax("com10"));
    }

    #[test]
    fn roots() {
        assert!(is_under_root("C:\\a\\", "c:\\A\\b"));
        assert!(is_under_root("C:\\a", "C:\\a"));
        assert!(!is_under_root("C:\\a", "C:\\ab"));
        assert_eq!(strip_extended_prefix("\\\\?\\UNC\\s\\x"), "\\\\s\\x");
        assert_eq!(join_below("C:\\r\\", "a/./b").as_deref(), Some("C:\\r\\a\\b"));
        assert_eq!(join_below("C:\\r", "."), None);
    }
}
