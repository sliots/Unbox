//! Line-delimited JSON protocol between the UI process and the extraction worker.
//! Both sides are the same executable, so the protocol is versioned with the binary.

use serde::{Deserialize, Serialize};

/// Messages written by the worker to its stdout.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerEvent {
    Init {
        total_items: u32,
        total_bytes: u64,
        size_known: bool,
        policy_filtered_items: u32,
        code_page: u32,
        budget_bytes: u64,
    },
    Progress {
        phase: Phase,
        current_file: String,
        completed_bytes: u64,
        total_bytes: u64,
        size_known: bool,
        /// Nesting depth of the archive being extracted: 0 for the source, 1 for an
        /// archive found inside it, and so on.
        #[serde(default, skip_serializing_if = "is_zero")]
        layer: u32,
        /// The split volume being read (1-based) out of `volumes`; 0 when not split.
        #[serde(default, skip_serializing_if = "is_zero")]
        volume: u32,
        #[serde(default, skip_serializing_if = "is_zero")]
        volumes: u32,
    },
    /// Ask the user for the password; `retry` after the previous answer was wrong.
    PasswordRequest {
        #[serde(default)]
        retry: bool,
        /// Wrong answers the user has given so far.
        #[serde(default, skip_serializing_if = "is_zero")]
        failures: u32,
        /// No failure was a definite "wrong password": the data may be damaged instead.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        maybe_damaged: bool,
        /// The archive was found inside the extracted content and can be left as it is
        /// (`PasswordSkip`).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        inner: bool,
    },
    /// Trying the `tried`-th of `total` automatic password guesses.
    PasswordProbing {
        tried: u32,
        total: u32,
    },
    InvalidCharsFound {
        items: Vec<InvalidItem>,
    },
    EncodingOptions {
        options: Vec<EncodingOption>,
    },
    PausedAck,
    ResumedAck,
    Canceled,
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_reason: Option<String>,
    },
    Finished(Box<FinishedReport>),
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// Messages written by the UI process to the worker's stdin.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerCommand {
    Start,
    Pause,
    Resume,
    Cancel,
    PasswordResponse {
        password: String,
    },
    /// Leave the inner archive that asked for a password as it is.
    PasswordSkip,
    InvalidCharsResponse {
        action: InvalidCharsAction,
    },
    EncodingResponse {
        cp: u32,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Outer,
    Inner,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InvalidCharsAction {
    BatchRename,
    Skip,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvalidItem {
    pub index: u32,
    pub raw: String,
    pub sanitized: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncodingOption {
    pub cp: u32,
    pub label: String,
    pub samples: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinishStatus {
    Ok,
    Partial,
    Failed,
}

impl FinishStatus {
    /// Exit code the worker uses for this status.
    pub fn exit_code(self) -> i32 {
        match self {
            FinishStatus::Ok => 0,
            FinishStatus::Failed => 10,
            FinishStatus::Partial => 11,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FinishedReport {
    pub status: FinishStatus,
    pub total_items: u32,
    pub extracted_items: u32,
    pub skipped_items: u32,
    pub policy_filtered_items: u32,
    pub unsafe_items: u32,
    pub missing_items: u32,
    pub completed_bytes: u64,
    pub empty_archive: bool,
    pub code_page: u32,
    /// Empty when there was no error.
    pub error_reason: String,
    pub phase: Phase,
    pub limit_bytes: u64,
    pub written_bytes: u64,
    pub intermediate_bytes: u64,
    pub size_known: bool,
    /// Delivery root relative to the staging directory; empty means the staging
    /// directory itself.
    pub delivery_root: String,
    pub manifest: String,
    pub manifest_ok: bool,
    pub opened_volumes: Vec<String>,
    pub volume_identities: Vec<VolumeIdentity>,
}

/// Object identity of one source volume: volume serial, 128-bit file id, size and
/// last-write time. Captured when the worker actually reads a volume and checked
/// again before recycling, so a replaced file is never recycled.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct VolumeIdentity {
    pub path: String,
    pub volume_serial: u64,
    #[serde(with = "hex16")]
    pub file_id: [u8; 16],
    pub size: u64,
    pub last_write_time: u64,
    pub valid: bool,
}

impl VolumeIdentity {
    pub fn matches(&self, current: &VolumeIdentity) -> bool {
        self.valid
            && current.valid
            && self.volume_serial == current.volume_serial
            && self.file_id == current.file_id
            && self.size == current.size
            && self.last_write_time == current.last_write_time
    }
}

mod hex16 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &[u8; 16], serializer: S) -> Result<S::Ok, S::Error> {
        let text: String = value.iter().map(|b| format!("{b:02x}")).collect();
        serializer.serialize_str(&text)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[u8; 16], D::Error> {
        let text = String::deserialize(deserializer)?;
        let mut out = [0u8; 16];
        if text.len() != 32 || !text.is_ascii() {
            return Err(serde::de::Error::custom("file id must be 32 hex digits"));
        }
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).map_err(serde::de::Error::custom)?;
        }
        Ok(out)
    }
}

/// Serialize one protocol line (without the trailing newline).
pub fn to_line<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("protocol messages always serialize")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let event = WorkerEvent::Error { message: "x".into(), error_reason: Some("crc_error".into()) };
        let line = to_line(&event);
        assert_eq!(line, r#"{"type":"error","message":"x","error_reason":"crc_error"}"#);
        assert_eq!(serde_json::from_str::<WorkerEvent>(&line).unwrap(), event);
        let cmd = WorkerCommand::InvalidCharsResponse { action: InvalidCharsAction::BatchRename };
        assert_eq!(to_line(&cmd), r#"{"type":"invalid_chars_response","action":"batch_rename"}"#);
        assert_eq!(to_line(&WorkerCommand::Start), r#"{"type":"start"}"#);
        assert_eq!(to_line(&WorkerCommand::PasswordSkip), r#"{"type":"password_skip"}"#);
        assert_eq!(
            serde_json::from_str::<WorkerEvent>(r#"{"type":"password_request"}"#).unwrap(),
            WorkerEvent::PasswordRequest { retry: false, failures: 0, maybe_damaged: false, inner: false }
        );
    }

    #[test]
    fn identity_hex() {
        let id = VolumeIdentity { file_id: [0xAB; 16], valid: true, ..Default::default() };
        let line = serde_json::to_string(&id).unwrap();
        assert!(line.contains("abababab"));
        let back: VolumeIdentity = serde_json::from_str(&line).unwrap();
        assert!(back.matches(&id));
    }
}
