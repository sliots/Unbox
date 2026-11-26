//! Reads undecoded entry names straight from ZIP central directories and TAR headers,
//! so declared Unicode names (ZIP bit 11, Unicode Path extra field, PAX `path`) are
//! honoured and only undeclared names go through code-page decoding.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawEntry {
    pub raw: Vec<u8>,
    pub declared_utf8: bool,
    pub is_dir: bool,
}

fn read_head(path: &Path, len: usize) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn le16(p: &[u8]) -> u64 {
    u16::from_le_bytes([p[0], p[1]]) as u64
}
fn le32(p: &[u8]) -> u64 {
    u32::from_le_bytes([p[0], p[1], p[2], p[3]]) as u64
}
fn le64(p: &[u8]) -> u64 {
    u64::from_le_bytes(p[..8].try_into().unwrap())
}

pub fn looks_like_zip(path: &Path) -> bool {
    read_head(path, 4).is_some_and(|s| s[0] == 0x50 && s[1] == 0x4B && matches!(s[2], 0x03 | 0x05 | 0x07))
}

pub fn looks_like_tar(path: &Path) -> bool {
    let Some(hdr) = read_head(path, 512) else { return false };
    &hdr[257..262] == b"ustar" || hdr.iter().all(|b| *b == 0) || tar_checksum_valid(&hdr)
}

/// Formats that are ZIP files underneath but are documents or packages in their own
/// right: a lone file like this is delivered as is, never unpacked.
const ZIP_CONTAINER_EXTENSIONS: &[&str] = &[
    "docx",
    "docm",
    "dotx",
    "xlsx",
    "xlsm",
    "xltx",
    "pptx",
    "pptm",
    "potx",
    "vsdx",
    "odt",
    "ods",
    "odp",
    "odg",
    "epub",
    "jar",
    "war",
    "ear",
    "apk",
    "aab",
    "xapk",
    "ipa",
    "xpi",
    "crx",
    "vsix",
    "nupkg",
    "whl",
    "appx",
    "msix",
    "appxbundle",
    "msixbundle",
    "3mf",
    "kmz",
    "usdz",
    "sketch",
    "ora",
    "cbz",
    "key",
    "pages",
    "numbers",
    "mcpack",
    "mcworld",
];

/// Comic-book archives are read as they are.
const KEEP_EXTENSIONS: &[&str] = &["cbr", "cb7", "cbt"];

/// The 7-Zip handler for a file that is an archive by content, whatever its name says
/// (shared archives are often renamed to hide them). Only real archive containers are
/// recognised: executables, disk images and ZIP-based documents are left alone.
pub fn sniff_archive(path: &Path) -> Option<&'static str> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if KEEP_EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    let mut head = Vec::with_capacity(512);
    File::open(path).ok()?.take(512).read_to_end(&mut head).ok()?;
    let starts = |magic: &[u8]| head.starts_with(magic);
    if starts(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]) {
        Some("7z")
    } else if starts(b"Rar!\x1A\x07\x01\x00") {
        Some("Rar5")
    } else if starts(b"Rar!\x1A\x07\x00") {
        Some("Rar")
    } else if starts(b"PK\x03\x04") || starts(b"PK\x05\x06") || starts(b"PK\x07\x08") {
        (!ZIP_CONTAINER_EXTENSIONS.contains(&ext.as_str())).then_some("zip")
    } else if starts(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]) {
        Some("xz")
    } else if starts(&[0x1F, 0x8B, 0x08]) {
        Some("gzip")
    } else if starts(b"BZh") && head.len() > 3 && (b'1'..=b'9').contains(&head[3]) {
        Some("bzip2")
    } else if starts(&[0x28, 0xB5, 0x2F, 0xFD]) {
        Some("zstd")
    } else if head.len() == 512 && (&head[257..262] == b"ustar" || (head.iter().any(|b| *b != 0) && tar_checksum_valid(&head))) {
        Some("tar")
    } else {
        None
    }
}

/// Total size a 7z archive declares in its start header (signature header plus the
/// offset and size of the end header), to tell a split set that lacks its last volumes.
pub fn sevenzip_declared_size(head: &[u8]) -> Option<u64> {
    if head.len() < 32 || !head.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]) {
        return None;
    }
    let field = |at: usize| u64::from_le_bytes(head[at..at + 8].try_into().unwrap());
    let (offset, size) = (field(12), field(20));
    if offset == 0 && size == 0 {
        return None;
    }
    32u64.checked_add(offset)?.checked_add(size)
}

fn parse_octal(p: &[u8]) -> u64 {
    if !p.is_empty() && p[0] & 0x80 != 0 {
        return p[1..].iter().fold((p[0] & 0x7F) as u64, |v, b| (v << 8) | *b as u64);
    }
    p.iter().filter(|b| (b'0'..=b'7').contains(b)).fold(0u64, |v, b| v.wrapping_mul(8).wrapping_add((b - b'0') as u64))
}

fn tar_checksum_valid(hdr: &[u8]) -> bool {
    let stored = parse_octal(&hdr[148..156]);
    let sum: u64 = hdr.iter().enumerate().map(|(i, b)| if (148..156).contains(&i) { b' ' as u64 } else { *b as u64 }).sum();
    sum == stored
}

fn trim_nul(mut v: Vec<u8>) -> Vec<u8> {
    while v.last() == Some(&0) {
        v.pop();
    }
    v
}

/// ZIP 0x7075 Unicode Path extra field (APPNOTE 4.6.9): only honoured when its CRC
/// matches the fallback name bytes.
fn zip_unicode_path(extra: &[u8], name_crc: u32) -> Option<Vec<u8>> {
    let mut off = 0usize;
    while off + 4 <= extra.len() {
        let id = le16(&extra[off..]);
        let size = le16(&extra[off + 2..]) as usize;
        off += 4;
        if off + size > extra.len() {
            return None;
        }
        if id == 0x7075 && size >= 5 {
            let version = extra[off];
            let crc = le32(&extra[off + 1..]) as u32;
            let name = trim_nul(extra[off + 5..off + size].to_vec());
            if version == 1 && crc == name_crc && !name.is_empty() {
                return Some(name);
            }
        }
        off += size;
    }
    None
}

/// Parse the central directory. `None` when the file is not a parsable ZIP or the
/// entry count does not add up.
pub fn read_zip_names(path: &Path) -> Option<Vec<RawEntry>> {
    let mut f = File::open(path).ok()?;
    let file_size = f.seek(SeekFrom::End(0)).ok()?;
    if file_size < 22 {
        return None;
    }
    let tail_len = file_size.min(66000);
    let mut tail = vec![0u8; tail_len as usize];
    f.seek(SeekFrom::Start(file_size - tail_len)).ok()?;
    f.read_exact(&mut tail).ok()?;

    let eocd = (0..=tail.len() - 22).rev().find(|&i| tail[i..i + 4] == [0x50, 0x4B, 0x05, 0x06])?;
    let mut entry_count = le16(&tail[eocd + 10..]);
    let mut cd_size = le32(&tail[eocd + 12..]);
    let mut cd_offset = le32(&tail[eocd + 16..]);

    if (entry_count == 0xFFFF || cd_offset == 0xFFFF_FFFF) && eocd >= 20 && le32(&tail[eocd - 20..]) == 0x0706_4B50 {
        {
            let z64_offset = le64(&tail[eocd - 20 + 8..]);
            let mut z64 = [0u8; 56];
            if f.seek(SeekFrom::Start(z64_offset)).is_ok() && f.read_exact(&mut z64).is_ok() && le32(&z64) == 0x0606_4B50 {
                entry_count = le64(&z64[32..]);
                cd_size = le64(&z64[40..]);
                cd_offset = le64(&z64[48..]);
            }
        }
    }

    if cd_offset == 0 || cd_size == 0 || cd_offset >= file_size || cd_size > 64 * 1024 * 1024 {
        return None;
    }
    let mut cd = vec![0u8; cd_size as usize];
    f.seek(SeekFrom::Start(cd_offset)).ok()?;
    f.read_exact(&mut cd).ok()?;

    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut parsed = 0u64;
    while pos + 46 <= cd.len() && parsed < entry_count {
        if le32(&cd[pos..]) != 0x0201_4B50 {
            break;
        }
        let flags = le16(&cd[pos + 8..]);
        let name_len = le16(&cd[pos + 28..]) as usize;
        let extra_len = le16(&cd[pos + 30..]) as usize;
        let comment_len = le16(&cd[pos + 32..]) as usize;
        if pos + 46 + name_len > cd.len() {
            break;
        }
        let raw = cd[pos + 46..pos + 46 + name_len].to_vec();
        let mut entry = RawEntry { is_dir: matches!(raw.last(), Some(b'/') | Some(b'\\')), declared_utf8: flags & 0x0800 != 0, raw };
        if !entry.declared_utf8 && extra_len > 0 && pos + 46 + name_len + extra_len <= cd.len() {
            let name_crc = crc32fast::hash(&entry.raw);
            if let Some(unicode) = zip_unicode_path(&cd[pos + 46 + name_len..pos + 46 + name_len + extra_len], name_crc) {
                entry.raw = unicode;
                entry.declared_utf8 = true;
            }
        }
        out.push(entry);
        pos += 46 + name_len + extra_len + comment_len;
        parsed += 1;
    }
    (!out.is_empty() && parsed == entry_count).then_some(out)
}

/// Iterate `<len> <key>=<value>\n` PAX records, keeping the last `path` and
/// `hdrcharset` values.
fn scan_pax(data: &[u8], path: &mut Option<Vec<u8>>, hdrcharset: &mut Vec<u8>) {
    let mut offset = 0usize;
    while offset + 2 <= data.len() {
        let Some(space) = data[offset..].iter().position(|b| *b == b' ').map(|p| p + offset) else { break };
        let Some(record_len) = std::str::from_utf8(&data[offset..space]).ok().and_then(|s| s.trim_start().parse::<usize>().ok()) else {
            break;
        };
        if record_len == 0 || offset + record_len > data.len() || space >= offset + record_len {
            break;
        }
        let record = &data[offset..offset + record_len];
        let key_start = space - offset + 1;
        if let Some(eq) = record[key_start..].iter().position(|b| *b == b'=').map(|p| p + key_start) {
            let key = &record[key_start..eq];
            let mut value = &record[eq + 1..];
            if value.last() == Some(&b'\n') {
                value = &value[..value.len() - 1];
            }
            match key {
                b"path" => *path = Some(value.to_vec()),
                b"hdrcharset" => *hdrcharset = value.to_vec(),
                _ => {}
            }
        }
        offset += record_len;
    }
}

const MAX_META: u64 = 64 * 1024 * 1024;

fn read_blocks<R: Read + Seek>(f: &mut R, size: u64) -> Option<Vec<u8>> {
    if size > MAX_META {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    f.read_exact(&mut data).ok()?;
    let pad = size.div_ceil(512) * 512 - size;
    if pad > 0 {
        f.seek(SeekFrom::Current(pad as i64)).ok()?;
    }
    Some(data)
}

/// Walk TAR headers (ustar prefix, GNU long names, local and global PAX headers).
pub fn read_tar_names(path: &Path) -> Option<Vec<RawEntry>> {
    let mut f = BufReader::new(File::open(path).ok()?);
    let mut out = Vec::new();
    let mut pending: Option<(Vec<u8>, bool)> = None;
    let mut global_charset: Vec<u8> = Vec::new();
    let mut zero_blocks = 0;

    for _ in 0..1_000_000 {
        let mut hdr = [0u8; 512];
        if f.read_exact(&mut hdr).is_err() {
            break;
        }
        if hdr.iter().all(|b| *b == 0) {
            zero_blocks += 1;
            if zero_blocks >= 2 {
                break;
            }
            continue;
        }
        zero_blocks = 0;
        let kind = hdr[156];
        let size = parse_octal(&hdr[124..136]);

        match kind {
            b'L' => {
                let Some(name) = read_blocks(&mut f, size) else { break };
                pending = Some((trim_nul(name), false));
                continue;
            }
            b'x' | b'X' => {
                let Some(data) = read_blocks(&mut f, size) else { break };
                let mut charset = global_charset.clone();
                let mut pax_path = None;
                scan_pax(&data, &mut pax_path, &mut charset);
                if let Some(p) = pax_path {
                    // Only hdrcharset=BINARY marks the path as raw bytes; PAX paths are UTF-8 otherwise.
                    pending = Some((p, charset != b"BINARY"));
                }
                continue;
            }
            b'g' => {
                let Some(data) = read_blocks(&mut f, size) else { break };
                let mut unused = None;
                scan_pax(&data, &mut unused, &mut global_charset);
                continue;
            }
            _ => {}
        }

        let (name, utf8) = match pending.take().filter(|(name, _)| !name.is_empty()) {
            Some(entry) => entry,
            None => {
                let name = trim_nul(hdr[..100].to_vec());
                let prefix = trim_nul(hdr[345..500].to_vec());
                let name = if prefix.is_empty() { name } else { [prefix, b"/".to_vec(), name].concat() };
                (name, false)
            }
        };
        if !name.is_empty() {
            out.push(RawEntry { raw: name, declared_utf8: utf8, is_dir: kind == b'5' });
        }
        let skip = size.div_ceil(512) * 512;
        if skip > 0 && f.seek(SeekFrom::Current(skip as i64)).is_err() {
            break;
        }
    }
    Some(out)
}

/// Raw names for a ZIP or TAR payload, identified by content.
pub fn read_raw_names(path: &Path) -> Option<Vec<RawEntry>> {
    if looks_like_zip(path) {
        read_zip_names(path)
    } else if looks_like_tar(path) {
        read_tar_names(path)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sevenzip_size_from_start_header() {
        let mut head = vec![0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0, 4];
        head.extend_from_slice(&[0; 4]); // start header CRC
        head.extend_from_slice(&1000u64.to_le_bytes());
        head.extend_from_slice(&90u64.to_le_bytes());
        head.extend_from_slice(&[0; 4]);
        assert_eq!(super::sevenzip_declared_size(&head), Some(1122));
        assert_eq!(super::sevenzip_declared_size(&head[..20]), None);
        assert_eq!(super::sevenzip_declared_size(b"PK\x03\x04"), None);
    }

    #[test]
    fn sniff_by_content_not_name() {
        let dir = std::env::temp_dir().join(format!("unbox-sniff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let p = dir.join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        };
        assert_eq!(sniff_archive(&write("movie.mp4", &[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0, 4])), Some("7z"));
        assert_eq!(sniff_archive(&write("a.jpg", b"Rar!\x1A\x07\x01\x00rest")), Some("Rar5"));
        assert_eq!(sniff_archive(&write("noext", b"PK\x03\x04....")), Some("zip"));
        assert_eq!(sniff_archive(&write("report.docx", b"PK\x03\x04....")), None);
        assert_eq!(sniff_archive(&write("book.cbr", b"Rar!\x1A\x07\x00")), None);
        assert_eq!(sniff_archive(&write("tool.exe", b"MZ\x90\x00")), None);
        assert_eq!(sniff_archive(&write("notes.txt", b"hello")), None);
        assert_eq!(sniff_archive(&write("zeros.bin", &[0u8; 1024])), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;
    use std::io::Write;

    fn temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("unbox-core-{}-{name}", std::process::id()));
        File::create(&path).unwrap().write_all(bytes).unwrap();
        path
    }

    fn tar_header(name: &[u8], kind: u8, size: u64) -> [u8; 512] {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name);
        h[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
        h[156] = kind;
        h[257..262].copy_from_slice(b"ustar");
        h
    }

    #[test]
    fn tar_pax_and_gnu_names() {
        let mut data = Vec::new();
        let pax = b"16 path=\xe4\xb8\xad.txt\n";
        data.extend_from_slice(&tar_header(b"pax", b'x', pax.len() as u64));
        let mut block = pax.to_vec();
        block.resize(512, 0);
        data.extend_from_slice(&block);
        data.extend_from_slice(&tar_header(b"ignored", b'0', 0));
        let long = b"\xd6\xd0.txt";
        data.extend_from_slice(&tar_header(b"././@LongLink", b'L', long.len() as u64));
        let mut block = long.to_vec();
        block.resize(512, 0);
        data.extend_from_slice(&block);
        data.extend_from_slice(&tar_header(b"short", b'0', 0));
        data.extend_from_slice(&[0u8; 1024]);
        let path = temp("t.tar", &data);
        assert!(looks_like_tar(&path));
        let names = read_tar_names(&path).unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0].raw, "中.txt".as_bytes());
        assert!(names[0].declared_utf8);
        assert_eq!(names[1].raw, long);
        assert!(!names[1].declared_utf8);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn zip_central_directory() {
        // One stored entry "a/" with bit 11 set, built by hand.
        let name = b"a/";
        let mut data = Vec::new();
        data.extend_from_slice(&[0x50, 0x4B, 0x03, 0x04]);
        data.extend_from_slice(&[0u8; 22]);
        data.extend_from_slice(&(name.len() as u16).to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(name);
        let cd_offset = data.len() as u32;
        let mut cd = vec![0x50, 0x4B, 0x01, 0x02];
        cd.extend_from_slice(&[0u8; 4]);
        cd.extend_from_slice(&0x0800u16.to_le_bytes());
        cd.extend_from_slice(&[0u8; 18]);
        cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
        cd.extend_from_slice(&[0u8; 16]);
        cd.extend_from_slice(name);
        data.extend_from_slice(&cd);
        let mut eocd = vec![0x50, 0x4B, 0x05, 0x06, 0, 0, 0, 0, 1, 0, 1, 0];
        eocd.extend_from_slice(&(cd.len() as u32).to_le_bytes());
        eocd.extend_from_slice(&cd_offset.to_le_bytes());
        eocd.extend_from_slice(&[0, 0]);
        data.extend_from_slice(&eocd);
        let path = temp("z.zip", &data);
        assert!(looks_like_zip(&path));
        let names = read_zip_names(&path).unwrap();
        assert_eq!(names, vec![RawEntry { raw: name.to_vec(), declared_utf8: true, is_dir: true }]);
        std::fs::remove_file(path).ok();
    }
}
