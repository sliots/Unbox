//! The archive format catalog (`assets/archive-formats.json`), shared by the
//! worker routing rules, the file picker and the file-association installer.

use serde::Deserialize;
use std::collections::HashSet;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum ArchiveKind {
    Archive,
    Stream,
    TarWrapper,
    Split,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveFormat {
    pub suffix: String,
    pub handler_hints: Vec<String>,
    pub kind: ArchiveKind,
    pub associate: bool,
    pub display_name: String,
    pub icon_key: String,
    pub badge_label: String,
    pub picker_group: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCatalog {
    schema_version: u32,
    formats: Vec<ArchiveFormat>,
}

/// The catalog embedded at build time.
pub const CATALOG_JSON: &str = include_str!("../../../assets/archive-formats.json");

/// Formats ordered by descending suffix length, so the longest suffix wins.
pub fn formats() -> &'static [ArchiveFormat] {
    static FORMATS: OnceLock<Vec<ArchiveFormat>> = OnceLock::new();
    FORMATS.get_or_init(|| parse(CATALOG_JSON).expect("embedded archive format catalog is invalid"))
}

fn is_suffix(value: &str) -> bool {
    // ^\.[a-z0-9]+(?:\.[a-z0-9]+)*$
    value.starts_with('.')
        && value[1..].split('.').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
}

fn is_key(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Parse and validate a catalog document.
pub fn parse(json: &str) -> Result<Vec<ArchiveFormat>, String> {
    let raw: RawCatalog = serde_json::from_str(json).map_err(|e| format!("invalid catalog: {e}"))?;
    if raw.schema_version != 1 || raw.formats.is_empty() {
        return Err("invalid archive format catalog schema".into());
    }
    let mut seen = HashSet::new();
    for format in &raw.formats {
        let valid = is_suffix(&format.suffix)
            && seen.insert(format.suffix.to_ascii_lowercase())
            && (!format.associate || format.suffix.matches('.').count() == 1)
            && is_key(&format.icon_key)
            && !format.handler_hints.is_empty()
            && format.handler_hints.iter().all(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_alphanumeric()))
            && !format.display_name.trim().is_empty()
            && !format.badge_label.trim().is_empty()
            && matches!(format.picker_group.as_str(), "Archive" | "Stream");
        if !valid {
            return Err(format!("invalid archive catalog entry {}", format.suffix));
        }
    }
    let mut formats = raw.formats;
    formats.sort_by(|a, b| b.suffix.len().cmp(&a.suffix.len()).then_with(|| a.suffix.cmp(&b.suffix)));
    Ok(formats)
}

/// Find the catalog entry whose suffix matches the end of `file_name`.
pub fn match_name(file_name: &str) -> Option<&'static ArchiveFormat> {
    formats().iter().find(|f| crate::ends_with_ignore_ascii_case(file_name, &f.suffix))
}

/// The formats that get a per-user file association.
pub fn associated() -> impl Iterator<Item = &'static ArchiveFormat> {
    formats().iter().filter(|f| f.associate)
}

/// Routing decision for one archive name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveRoute {
    pub kind: ArchiveKind,
    pub suffix: String,
    pub base_name: String,
    pub handler_hints: Vec<String>,
}

impl Default for ArchiveRoute {
    fn default() -> Self {
        Self { kind: ArchiveKind::Archive, suffix: String::new(), base_name: String::new(), handler_hints: Vec::new() }
    }
}

pub fn classify(name: &str) -> ArchiveRoute {
    match match_name(name) {
        Some(format) => ArchiveRoute {
            kind: format.kind,
            suffix: format.suffix.clone(),
            base_name: name[..name.len() - format.suffix.len()].to_string(),
            handler_hints: format.handler_hints.clone(),
        },
        None => ArchiveRoute::default(),
    }
}

/// Name used for a single unnamed stream inside a compressed file.
pub fn default_stream_name(route: &ArchiveRoute, leaf: &str) -> String {
    if route.kind == ArchiveKind::TarWrapper && !route.base_name.is_empty() {
        return format!("{}.tar", route.base_name);
    }
    if matches!(route.kind, ArchiveKind::Stream | ArchiveKind::Split) {
        if let Some(dot) = leaf.rfind('.') {
            if dot != 0 {
                return leaf[..dot].to_string();
            }
        }
    }
    "extracted_file".to_string()
}

/// Extensions (without the leading dot) for the "open archive" picker, grouped
/// as (label, extensions).
pub fn picker_filters() -> Vec<(&'static str, Vec<String>)> {
    let group =
        |name: &str| -> Vec<String> { formats().iter().filter(|f| f.picker_group == name).map(|f| f.suffix[1..].to_string()).collect() };
    let archive = group("Archive");
    let stream = group("Stream");
    let mut all = archive.clone();
    all.extend(stream.iter().cloned());
    vec![("支持的压缩文件", all), ("归档与分卷", archive), ("压缩流与复合 TAR", stream)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_is_valid() {
        assert!(formats().len() > 50);
        assert_eq!(associated().count(), 57);
    }

    #[test]
    fn longest_suffix_wins() {
        let route = classify("Backup.TAR.GZ");
        assert_eq!(route.kind, ArchiveKind::TarWrapper);
        assert_eq!(route.base_name, "Backup");
        assert_eq!(route.handler_hints, vec!["gzip"]);
        assert_eq!(classify("a.gz").kind, ArchiveKind::Stream);
        assert_eq!(classify("a.unknown"), ArchiveRoute::default());
    }

    #[test]
    fn stream_names() {
        assert_eq!(default_stream_name(&classify("x.tar.zst"), "x.tar.zst"), "x.tar");
        assert_eq!(default_stream_name(&classify("doc.txt.gz"), "doc.txt.gz"), "doc.txt");
        assert_eq!(default_stream_name(&classify("a.7z"), "a.7z"), "extracted_file");
        assert_eq!(default_stream_name(&classify(".gz"), ".gz"), "extracted_file");
    }

    #[test]
    fn rejects_bad_entries() {
        let bad = r#"{"schemaVersion":1,"formats":[{"suffix":".ZIP","handlerHints":["zip"],"kind":"Archive","associate":true,"displayName":"x","iconKey":"zip","badgeLabel":"ZIP","pickerGroup":"Archive"}]}"#;
        assert!(parse(bad).is_err());
    }
}
