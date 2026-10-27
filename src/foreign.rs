//! 다른 프로그램이 만든 압축 파일 풀기: zip, 7z, tar, tar.gz/bz2/xz/zst, 단일 .gz/.bz2/.xz/.zst
//!
//! 형식은 확장자가 아니라 파일 맨 앞의 "매직 넘버"로 알아낸다 (확장자가 틀려도 동작).
//! 경로 검사·이름 충돌·쓰기는 Pouch 포맷과 같은 [`Sink`]를 쓴다.

use crate::format::{self, EntryHeader, EntryKind};
use crate::sink::Sink;
use crate::{ArchiveInfo, EntryInfo};
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 압축 파일 종류
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Pouch,
    Zip,
    SevenZ,
    /// tar (+ 바깥 압축)
    Tar(Outer),
    /// 파일 하나만 압축한 .gz / .bz2 / .xz / .zst
    Single(Outer),
}

/// tar나 파일 하나를 감싸는 스트림 압축
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outer {
    None,
    Gz,
    Bz2,
    Xz,
    Zst,
}

impl ArchiveKind {
    pub fn label(self) -> &'static str {
        match self {
            ArchiveKind::Pouch => "pouch",
            ArchiveKind::Zip => "zip",
            ArchiveKind::SevenZ => "7z",
            ArchiveKind::Tar(Outer::None) => "tar",
            ArchiveKind::Tar(Outer::Gz) => "tar.gz",
            ArchiveKind::Tar(Outer::Bz2) => "tar.bz2",
            ArchiveKind::Tar(Outer::Xz) => "tar.xz",
            ArchiveKind::Tar(Outer::Zst) => "tar.zst",
            ArchiveKind::Single(Outer::Gz) => "gz",
            ArchiveKind::Single(Outer::Bz2) => "bz2",
            ArchiveKind::Single(Outer::Xz) => "xz",
            ArchiveKind::Single(Outer::Zst) => "zst",
            ArchiveKind::Single(Outer::None) => "?",
        }
    }
}

/// 이 프로그램이 열 수 있는 확장자 (파일 선택 창·끌어다 놓기 판단용)
pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "pouch", "bzx", "zip", "7z", "tar", "gz", "tgz", "bz2", "tbz", "tbz2", "xz", "txz", "zst",
    "tzst",
];

/// 확장자로 보아 압축 파일인지 (빠른 판단용, 실제 형식은 [`detect`]가 확인)
pub fn has_archive_extension(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        SUPPORTED_EXTENSIONS
            .iter()
            .any(|s| s.eq_ignore_ascii_case(e))
    })
}

/// 파일 앞부분의 매직 넘버로 형식을 알아낸다
pub fn detect(path: &Path) -> Result<ArchiveKind> {
    let mut f =
        File::open(path).with_context(|| format!("파일을 열 수 없음: {}", path.display()))?;
    let mut head = [0u8; 512];
    let n = read_up_to(&mut f, &mut head)?;
    let head = &head[..n];

    let outer = if head.starts_with(&format::MAGIC) || head.starts_with(&format::LEGACY_MAGIC) {
        return Ok(ArchiveKind::Pouch);
    } else if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        return Ok(ArchiveKind::Zip);
    } else if head.starts_with(&[b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C]) {
        return Ok(ArchiveKind::SevenZ);
    } else if head.starts_with(&[0x1F, 0x8B]) {
        Outer::Gz
    } else if head.starts_with(b"BZh") {
        Outer::Bz2
    } else if head.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]) {
        Outer::Xz
    } else if head.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        Outer::Zst
    } else if is_tar_header(head) {
        return Ok(ArchiveKind::Tar(Outer::None));
    } else {
        bail!("지원하지 않는 압축 형식입니다 (지원: pouch, zip, 7z, tar, gz, bz2, xz, zst)");
    };

    // 압축 스트림 안이 tar인지, 파일 하나인지 앞부분을 풀어서 확인
    let mut inner = open_outer(path, outer)?;
    let mut peek = [0u8; 512];
    let n = read_up_to(&mut inner, &mut peek).unwrap_or(0);
    if is_tar_header(&peek[..n]) {
        Ok(ArchiveKind::Tar(outer))
    } else {
        Ok(ArchiveKind::Single(outer))
    }
}

fn is_tar_header(block: &[u8]) -> bool {
    block.len() >= 262 && &block[257..262] == b"ustar"
}

fn read_up_to(r: &mut dyn Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// 바깥 압축을 푸는 Reader를 연다
fn open_outer(path: &Path, outer: Outer) -> Result<Box<dyn Read>> {
    let f = BufReader::with_capacity(crate::BUF_SIZE, File::open(path)?);
    Ok(match outer {
        Outer::None => Box::new(f),
        Outer::Gz => Box::new(flate2::bufread::MultiGzDecoder::new(f)),
        Outer::Bz2 => Box::new(bzip2::bufread::MultiBzDecoder::new(f)),
        Outer::Xz => Box::new(lzma_rust2::XzReader::new(f, true)),
        Outer::Zst => Box::new(zstd::stream::Decoder::with_buffer(f)?),
    })
}

// ───────────────────────────── 풀기 ─────────────────────────────

pub(crate) fn extract(
    input: &Path,
    kind: ArchiveKind,
    sink: &mut Sink,
    on_entry: impl FnMut(EntryInfo),
) -> Result<ArchiveInfo> {
    let (entries, total) = match kind {
        ArchiveKind::Zip => extract_zip(input, sink, on_entry)?,
        ArchiveKind::SevenZ => extract_7z(input, sink, on_entry)?,
        ArchiveKind::Tar(outer) => extract_tar(input, outer, sink, on_entry)?,
        ArchiveKind::Single(outer) => extract_single(input, outer, sink, on_entry)?,
        ArchiveKind::Pouch => unreachable!("Pouch 포맷은 native 모듈이 처리"),
    };
    Ok(ArchiveInfo {
        kind,
        entry_count: entries,
        total_size: total,
    })
}

fn header(kind: EntryKind, path: &str, size: u64, mtime: Option<SystemTime>) -> EntryHeader {
    let (secs, nanos) = format::split_time(mtime.unwrap_or(UNIX_EPOCH));
    EntryHeader {
        kind,
        path: path.to_string(),
        mtime_secs: secs,
        mtime_nanos: nanos,
        size,
    }
}

/// zip 안의 파일 이름을 읽는다.
/// UTF-8이면 그대로, 아니면 한국어 윈도우 기본 인코딩(CP949)으로 본다.
/// (윈도우 기본 압축 기능 등으로 만든 한글 zip은 이름이 CP949로 들어 있다)
fn zip_name(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        Err(_) => encoding_rs::EUC_KR.decode(raw).0.into_owned(),
    }
}

/// zip의 수정 시각: 정확한 UTC 시각(확장 타임스탬프)이 있으면 그걸, 없으면 DOS 시각(현지 시간)을 쓴다
fn zip_mtime(ext_time: Option<u32>, dos: Option<zip::DateTime>) -> Option<SystemTime> {
    if let Some(t) = ext_time {
        return Some(UNIX_EPOCH + Duration::from_secs(t as u64));
    }
    use chrono::TimeZone;
    let d = dos?;
    let naive = chrono::NaiveDate::from_ymd_opt(d.year() as i32, d.month() as u32, d.day() as u32)?
        .and_hms_opt(d.hour() as u32, d.minute() as u32, d.second() as u32)?;
    let local = chrono::Local.from_local_datetime(&naive).earliest()?;
    let secs = local.timestamp();
    (secs >= 0).then(|| UNIX_EPOCH + Duration::from_secs(secs as u64))
}

fn zip_ext_time(
    fields: impl Iterator<Item = impl std::borrow::Borrow<zip::ExtraField>>,
) -> Option<u32> {
    fields.into_iter().find_map(|f| match f.borrow() {
        zip::ExtraField::ExtendedTimestamp(ts) => ts.mod_time(),
        _ => None,
    })
}

fn zip_error(e: zip::result::ZipError) -> anyhow::Error {
    let msg = e.to_string();
    if msg.to_lowercase().contains("password") {
        anyhow::anyhow!("암호가 걸린 zip은 아직 지원하지 않습니다")
    } else {
        anyhow::anyhow!("zip 읽기 오류: {msg}")
    }
}

fn extract_zip(
    input: &Path,
    sink: &mut Sink,
    mut on_entry: impl FnMut(EntryInfo),
) -> Result<(u64, u64)> {
    let f = BufReader::with_capacity(crate::BUF_SIZE, File::open(input)?);
    let mut zip = zip::ZipArchive::new(f).map_err(zip_error)?;
    let mut total = 0;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).map_err(zip_error)?;
        let name = format::normalize_foreign_path(&zip_name(file.name_raw()));
        if name.is_empty() {
            continue;
        }
        let mtime = zip_mtime(zip_ext_time(file.extra_data_fields()), file.last_modified());
        if file.is_symlink() {
            sink.skip(&name);
            continue;
        }
        if file.is_dir() {
            on_entry(EntryInfo {
                header: &header(EntryKind::Dir, &name, 0, mtime),
            });
            sink.dir(&name, mtime)?;
        } else {
            let size = file.size();
            on_entry(EntryInfo {
                header: &header(EntryKind::File, &name, size, mtime),
            });
            // zip 라이브러리가 파일 끝에서 CRC32를 검사해 손상되면 오류를 낸다
            sink.file(&name, size, mtime, &mut file)
                .with_context(|| format!("zip 안의 파일을 풀 수 없음: {name}"))?;
            total += size;
        }
    }
    Ok((zip.len() as u64, total))
}

fn extract_7z(
    input: &Path,
    sink: &mut Sink,
    mut on_entry: impl FnMut(EntryInfo),
) -> Result<(u64, u64)> {
    let mut reader =
        sevenz_rust2::ArchiveReader::new(File::open(input)?, sevenz_rust2::Password::empty())
            .map_err(|e| anyhow::anyhow!("7z 읽기 오류: {e}"))?;
    reader.set_thread_count(crate::num_threads());
    let mut failure: Option<anyhow::Error> = None;
    let mut count = 0u64;
    let mut total = 0u64;
    let result = reader.for_each_entries(|entry, data| {
        if entry.is_anti_item() {
            return Ok(true);
        }
        let name = format::normalize_foreign_path(entry.name());
        if name.is_empty() {
            return Ok(true);
        }
        let mtime = entry
            .has_last_modified_date
            .then(|| SystemTime::from(entry.last_modified_date()));
        count += 1;
        let r = if entry.is_directory() {
            on_entry(EntryInfo {
                header: &header(EntryKind::Dir, &name, 0, mtime),
            });
            sink.dir(&name, mtime)
        } else {
            on_entry(EntryInfo {
                header: &header(EntryKind::File, &name, entry.size(), mtime),
            });
            total += entry.size();
            sink.file(&name, entry.size(), mtime, data).map(|_| ())
        };
        match r {
            Ok(()) => Ok(true),
            Err(e) => {
                failure = Some(e);
                Ok(false) // 멈춤
            }
        }
    });
    if let Some(e) = failure {
        return Err(e);
    }
    result.map_err(|e| {
        let msg = e.to_string();
        if msg.to_lowercase().contains("password") {
            anyhow::anyhow!("암호가 걸린 7z는 아직 지원하지 않습니다")
        } else {
            anyhow::anyhow!("7z 풀기 오류: {msg}")
        }
    })?;
    Ok((count, total))
}

fn extract_tar(
    input: &Path,
    outer: Outer,
    sink: &mut Sink,
    mut on_entry: impl FnMut(EntryInfo),
) -> Result<(u64, u64)> {
    let mut archive = tar::Archive::new(open_outer(input, outer)?);
    let mut count = 0u64;
    let mut total = 0u64;
    for entry in archive.entries().context("tar 읽기 오류")? {
        let mut entry = entry.context("tar 읽기 오류 (파일 손상)")?;
        let raw_name = entry.path_bytes().into_owned();
        let name = format::normalize_foreign_path(&String::from_utf8_lossy(&raw_name));
        if name.is_empty() {
            continue;
        }
        let mtime = entry
            .header()
            .mtime()
            .ok()
            .map(|s| UNIX_EPOCH + Duration::from_secs(s));
        let et = entry.header().entry_type();
        if et.is_dir() {
            count += 1;
            on_entry(EntryInfo {
                header: &header(EntryKind::Dir, &name, 0, mtime),
            });
            sink.dir(&name, mtime)?;
        } else if et.is_file() || et == tar::EntryType::Continuous {
            count += 1;
            let size = entry.size();
            on_entry(EntryInfo {
                header: &header(EntryKind::File, &name, size, mtime),
            });
            sink.file(&name, size, mtime, &mut entry)?;
            total += size;
        } else if et.is_symlink() || et.is_hard_link() {
            sink.skip(&name);
        }
        // 그 밖의 종류(장치 파일, 확장 헤더 등)는 무시
    }
    Ok((count, total))
}

fn extract_single(
    input: &Path,
    outer: Outer,
    sink: &mut Sink,
    mut on_entry: impl FnMut(EntryInfo),
) -> Result<(u64, u64)> {
    // "보고서.txt.gz" → "보고서.txt"
    let name = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "output".into());
    let name = format::normalize_foreign_path(&name);
    let mtime = std::fs::metadata(input).and_then(|m| m.modified()).ok();
    let mut r = open_outer(input, outer)?;
    on_entry(EntryInfo {
        header: &header(EntryKind::File, &name, 0, mtime),
    });
    // 크기를 미리 모르므로 큰 파일로 취급 (메모리에 모으지 않고 바로 씀)
    let (bytes, _) = sink.file(&name, u64::MAX, mtime, &mut r)?;
    Ok((1, bytes))
}

// ───────────────────────────── 목록 (빠르게) ─────────────────────────────

/// zip·7z는 파일 끝의 목차만 읽어서 목록을 바로 만든다 (풀지 않음)
pub(crate) fn list_fast(
    input: &Path,
    kind: ArchiveKind,
) -> Result<(ArchiveInfo, Vec<EntryHeader>)> {
    let mut entries = Vec::new();
    let mut total = 0u64;
    match kind {
        ArchiveKind::Zip => {
            let f = BufReader::new(File::open(input)?);
            let zip = zip::ZipArchive::new(f).map_err(zip_error)?;
            for i in 0..zip.len() {
                let e = zip.by_index_data(i).map_err(zip_error)?;
                let name = format::normalize_foreign_path(&zip_name(e.name_raw()));
                if name.is_empty() || e.is_symlink() {
                    continue;
                }
                let mtime = zip_mtime(None, e.last_modified());
                if e.is_dir() {
                    entries.push(header(EntryKind::Dir, &name, 0, mtime));
                } else {
                    total += e.size();
                    entries.push(header(EntryKind::File, &name, e.size(), mtime));
                }
            }
        }
        ArchiveKind::SevenZ => {
            let reader = sevenz_rust2::ArchiveReader::new(
                File::open(input)?,
                sevenz_rust2::Password::empty(),
            )
            .map_err(|e| anyhow::anyhow!("7z 읽기 오류: {e}"))?;
            for e in &reader.archive().files {
                if e.is_anti_item() {
                    continue;
                }
                let name = format::normalize_foreign_path(e.name());
                if name.is_empty() {
                    continue;
                }
                let mtime = e
                    .has_last_modified_date
                    .then(|| SystemTime::from(e.last_modified_date()));
                if e.is_directory() {
                    entries.push(header(EntryKind::Dir, &name, 0, mtime));
                } else {
                    total += e.size();
                    entries.push(header(EntryKind::File, &name, e.size(), mtime));
                }
            }
        }
        _ => bail!("목차만 읽을 수 없는 형식입니다"),
    }
    let info = ArchiveInfo {
        kind,
        entry_count: entries.len() as u64,
        total_size: total,
    };
    Ok((info, entries))
}
