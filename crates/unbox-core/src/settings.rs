//! Persisted user preferences. The JSON layout is compatible with the settings file
//! written by Unbox 1.x (`%LOCALAPPDATA%\UnzipProxy\settings.json`).

use crate::ipc::InvalidCharsAction;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase", default)]
pub struct UserSettings {
    pub recycle_on_success: bool,
    /// `ask`, `batch_rename` or `skip`.
    pub invalid_chars_action: String,
    pub remember_invalid_chars_action: bool,
    pub is_details_expanded: bool,
    /// 0 means automatic (system ANSI code page).
    pub archive_code_page: u32,
    pub remember_archive_code_page: bool,
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            recycle_on_success: false,
            invalid_chars_action: "ask".into(),
            remember_invalid_chars_action: false,
            is_details_expanded: false,
            archive_code_page: 0,
            remember_archive_code_page: false,
        }
    }
}

impl UserSettings {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
    }

    /// Best effort: a failed write never affects extraction.
    pub fn save(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let temp = path.with_extension("json.tmp");
            if std::fs::write(&temp, text).is_ok() && std::fs::rename(&temp, path).is_err() {
                let _ = std::fs::remove_file(&temp);
            }
        }
    }

    /// The remembered answer to the illegal-name prompt, if any.
    pub fn remembered_invalid_action(&self) -> Option<InvalidCharsAction> {
        if !self.remember_invalid_chars_action {
            return None;
        }
        match self.invalid_chars_action.as_str() {
            "batch_rename" => Some(InvalidCharsAction::BatchRename),
            "skip" => Some(InvalidCharsAction::Skip),
            _ => None,
        }
    }

    pub fn remember_invalid_action(&mut self, action: InvalidCharsAction) {
        self.invalid_chars_action = match action {
            InvalidCharsAction::BatchRename => "batch_rename",
            InvalidCharsAction::Skip => "skip",
        }
        .into();
        self.remember_invalid_chars_action = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_legacy_file() {
        let legacy = r#"{ "RecycleOnSuccess": true, "InvalidCharsAction": "skip", "RememberInvalidCharsAction": true, "IsDetailsExpanded": true, "ArchiveCodePage": 936, "RememberArchiveCodePage": false }"#;
        let s: UserSettings = serde_json::from_str(legacy).unwrap();
        assert!(s.recycle_on_success && s.is_details_expanded);
        assert_eq!(s.remembered_invalid_action(), Some(InvalidCharsAction::Skip));
        let partial: UserSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(partial, UserSettings::default());
    }
}
