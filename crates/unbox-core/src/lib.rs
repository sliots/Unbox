//! Platform-independent rules shared by the Unbox UI process and its extraction worker.
//!
//! Everything in this crate is plain Rust so it can be unit-tested on any host; the
//! Windows-specific pieces (7-Zip COM, handle-pinned file system access, Shell recycling)
//! live in the `unbox` crate and call into these rules.

pub mod budget;
pub mod catalog;
pub mod decode;
pub mod ipc;
pub mod lz4;
pub mod manifest;
pub mod naming;
pub mod progress;
pub mod raw_names;
pub mod sanitize;
pub mod settings;
pub mod text;

/// Case-insensitive comparison used for Windows paths and names.
pub fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.eq_ignore_ascii_case(b) || fold(a) == fold(b)
}

/// Case fold used as a dictionary key for Windows names.
pub fn fold(value: &str) -> String {
    value.to_uppercase()
}

/// `true` when `value` ends with the ASCII `suffix`, ignoring ASCII case.
pub fn ends_with_ignore_ascii_case(value: &str, suffix: &str) -> bool {
    value.len() >= suffix.len()
        && value.is_char_boundary(value.len() - suffix.len())
        && value[value.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

/// `true` when `value` starts with `prefix`, ignoring case.
pub fn starts_with_ignore_case(value: &str, prefix: &str) -> bool {
    if value.len() >= prefix.len() && value.is_char_boundary(prefix.len()) && value[..prefix.len()].eq_ignore_ascii_case(prefix) {
        return true;
    }
    let mut chars = value.chars();
    for expected in prefix.chars() {
        match chars.next() {
            Some(actual) if actual.to_uppercase().eq(expected.to_uppercase()) => {}
            _ => return false,
        }
    }
    true
}
