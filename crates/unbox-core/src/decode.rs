//! File name decoding. Names are only re-decoded when the archive does not declare a
//! reliable encoding, and the one chosen name is used for display, writing and
//! verification alike.

pub const UTF8: u32 = 65001;

/// Converts legacy multi-byte names. On Windows this is `MultiByteToWideChar`.
pub trait CodePages {
    /// Decode `bytes` in code page `cp`. The flag is `false` when some bytes could not
    /// be represented (the returned text then contains substitutes, or is empty).
    fn decode(&self, bytes: &[u8], cp: u32) -> (String, bool);
}

/// Strict UTF-8 check (rejects overlong forms and surrogates). Returns
/// `(valid, has_non_ascii)`, where `has_non_ascii` reports whether a valid multi-byte
/// sequence was seen before the scan stopped (the original worker's semantics).
pub fn utf8_validity(bytes: &[u8]) -> (bool, bool) {
    let mut non_ascii = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c < 0x80 {
            i += 1;
            continue;
        }
        let (extra, mut cp) = if c & 0xE0 == 0xC0 {
            (1, (c & 0x1F) as u32)
        } else if c & 0xF0 == 0xE0 {
            (2, (c & 0x0F) as u32)
        } else if c & 0xF8 == 0xF0 {
            (3, (c & 0x07) as u32)
        } else {
            return (false, non_ascii);
        };
        if i + extra >= bytes.len() {
            return (false, non_ascii);
        }
        for k in 1..=extra {
            let cc = bytes[i + k];
            if cc & 0xC0 != 0x80 {
                return (false, non_ascii);
            }
            cp = (cp << 6) | (cc & 0x3F) as u32;
        }
        if (extra == 1 && cp < 0x80) || (extra == 2 && cp < 0x800) || (extra == 3 && cp < 0x10000) {
            return (false, non_ascii);
        }
        if (0xD800..=0xDFFF).contains(&cp) || cp > 0x10FFFF {
            return (false, non_ascii);
        }
        non_ascii = true;
        i += extra + 1;
    }
    (true, non_ascii)
}

/// Whether the bytes contain a 3- or 4-byte UTF-8 sequence (the strongest signal of
/// real CJK text). Mirrors the scanner of the original worker.
pub fn has_multibyte_utf8(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c < 0x80 {
            i += 1;
            continue;
        }
        if c & 0xE0 == 0xC0 {
            i += 2;
            continue;
        }
        return c & 0xF0 == 0xE0 || c & 0xF8 == 0xF0;
    }
    false
}

fn contains_replacement(text: &str) -> bool {
    text.contains('\u{FFFD}')
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Decoded {
    pub name: String,
    /// The encoding is uncertain and the user should confirm it.
    pub ambiguous: bool,
    /// Some bytes could not be represented.
    pub undecodable: bool,
}

/// Decode one archive name.
///
/// * `declared_utf8` – the format declares the name as Unicode; it is honoured.
/// * `strict` – the user explicitly chose `cp`; legacy names are decoded with it and no
///   heuristic may override the choice.
pub fn decode_one(cps: &dyn CodePages, bytes: &[u8], declared_utf8: bool, cp: u32, strict: bool) -> Decoded {
    let mut result = Decoded::default();
    if bytes.is_empty() {
        return result;
    }
    let (utf8_valid, non_ascii) = utf8_validity(bytes);
    let (primary, primary_clean) = cps.decode(bytes, cp);

    if declared_utf8 {
        if utf8_valid {
            result.name = String::from_utf8_lossy(bytes).into_owned();
            return result;
        }
        result.name = primary;
        result.ambiguous = true;
        result.undecodable = !primary_clean;
        return result;
    }

    if strict {
        result.name = primary;
        result.undecodable = !primary_clean;
        result.ambiguous = !primary_clean;
        return result;
    }

    if utf8_valid && non_ascii {
        let as_utf8 = String::from_utf8_lossy(bytes).into_owned();
        if has_multibyte_utf8(bytes) {
            result.name = as_utf8;
            return result;
        }
        result.name = primary;
        if !primary_clean {
            result.name = as_utf8;
            result.ambiguous = true;
            result.undecodable = true;
        }
        return result;
    }

    result.undecodable = !primary_clean;
    result.ambiguous = !primary_clean && !contains_replacement(&primary);
    result.name = primary;
    result
}

/// Code pages offered when the encoding is uncertain; the first is the default.
pub fn candidate_code_pages(preferred: u32) -> Vec<u32> {
    let mut list = Vec::new();
    for cp in [preferred, UTF8, 936, 950, 932, 949, 1252, 437] {
        if cp != 0 && !list.contains(&cp) {
            list.push(cp);
        }
    }
    list
}

pub fn code_page_label(cp: u32) -> String {
    match cp {
        65001 => "UTF-8".into(),
        936 => "简体中文 (GBK, CP936)".into(),
        950 => "繁体中文 (Big5, CP950)".into(),
        932 => "日文 (Shift-JIS, CP932)".into(),
        949 => "韩文 (EUC-KR, CP949)".into(),
        1252 => "西欧 (Windows-1252)".into(),
        437 => "DOS 英文 (CP437)".into(),
        _ => format!("代码页 {cp}"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Test double: UTF-8 is strict, everything else is Latin-1 except 936 which
    /// rejects lone high bytes at the end.
    pub struct FakeCodePages;
    impl CodePages for FakeCodePages {
        fn decode(&self, bytes: &[u8], cp: u32) -> (String, bool) {
            match cp {
                UTF8 => match std::str::from_utf8(bytes) {
                    Ok(t) => (t.to_string(), true),
                    Err(_) => (String::from_utf8_lossy(bytes).into_owned(), false),
                },
                936 if bytes.last().is_some_and(|b| *b >= 0x80) && bytes.len() % 2 == 1 => {
                    (String::from_utf8_lossy(bytes).into_owned(), false)
                }
                _ => (bytes.iter().map(|b| *b as char).collect(), true),
            }
        }
    }

    #[test]
    fn declared_utf8_is_kept() {
        let d = decode_one(&FakeCodePages, "中文.txt".as_bytes(), true, 936, false);
        assert_eq!(d.name, "中文.txt");
        assert!(!d.ambiguous);
    }

    #[test]
    fn three_byte_utf8_is_detected() {
        let d = decode_one(&FakeCodePages, "中文.txt".as_bytes(), false, 1252, false);
        assert_eq!(d.name, "中文.txt");
    }

    #[test]
    fn strict_mode_uses_choice() {
        let d = decode_one(&FakeCodePages, "中文.txt".as_bytes(), false, 1252, true);
        assert_ne!(d.name, "中文.txt");
        assert!(!d.ambiguous);
    }

    #[test]
    fn candidates_are_unique() {
        assert_eq!(candidate_code_pages(936), vec![936, 65001, 950, 932, 949, 1252, 437]);
    }

    #[test]
    fn multibyte_scanner() {
        assert!(has_multibyte_utf8("é中".as_bytes()));
        assert!(!has_multibyte_utf8("é".as_bytes()));
    }
}
