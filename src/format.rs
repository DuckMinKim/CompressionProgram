//! .bzx 아카이브 포맷 (Pouch 자체 형식) (버전 3, 버전 2 읽기 지원)
//!
//! ```text
//! [헤더 32바이트 — 압축 안 됨]
//! offset  size  내용
//! 0       4     매직 넘버 "BZX\0" (잠시 .pouch를 쓰던 때 파일은 "PCH\0"도 읽음)
//! 4       1     포맷 버전 (3)
//! 5       1     코덱 ID (1 = zstd)
//! 6       1     압축 레벨
//! 7       1     예약 (0)
//! 8       8     항목 수 (파일 + 폴더, u64 LE)
//! 16      8     원본 전체 크기 (u64 LE)
//! 24      8     블록 인덱스 위치 (u64 LE, v3) / 예약 (v2)
//!
//! [본문 — "항목 스트림"]
//! 항목이 차례로 이어지고, 마지막에 End 항목(종류 0)이 온다.
//!
//!   종류 (u8)          0 = End, 1 = 파일, 2 = 폴더
//!   경로 길이 (u16 LE)
//!   경로 (UTF-8)       아카이브 안의 상대 경로, 구분자는 항상 '/'
//!   수정 시각 초 (i64 LE)    UNIX 시간
//!   수정 시각 나노초 (u32 LE)
//!   ── 파일일 때만 ──
//!   크기 (u64 LE)
//!   데이터 (크기만큼)
//!   CRC32 (u32 LE)
//!
//! v2: 항목 스트림 전체가 zstd 스트림 하나 (해제는 한 스레드로만 가능)
//! v3: 항목 스트림을 4MB 블록으로 잘라 블록마다 독립적으로 압축하고 이어 붙인다.
//!     블록이 서로 독립적이라 압축도 해제도 CPU 코어 수만큼 동시에 할 수 있다.
//!     이미 압축된 데이터(사진·영상 등)로 보이는 블록은 압축하지 않고 그대로 저장한다.
//!
//! [블록 인덱스 — v3, 파일 맨 끝]
//!   "BZXI" (4)  블록 수 (u32 LE)
//!   블록마다: 압축 크기 (u32) · 원본 크기 (u32) · 플래그 (u8: 0 = zstd, 1 = 그대로 저장)
//! ```
//!
//! 블록은 헤더 바로 뒤부터 빈틈없이 이어지므로, 블록 크기만 알면 위치를 계산할 수 있다.
//! 항목 수·전체 크기·인덱스 위치는 압축이 끝나야 알 수 있으므로, 마지막에 헤더로 되돌아가 채운다.

use anyhow::{Result, bail};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const MAGIC: [u8; 4] = *b"BZX\0";
/// 잠시 .pouch 확장자를 쓰던 때 만든 파일의 매직 넘버 — 계속 읽을 수 있다
pub const LEGACY_MAGIC: [u8; 4] = *b"PCH\0";
/// 새로 만드는 파일의 버전
pub const VERSION: u8 = 3;
/// 읽을 수 있는 가장 오래된 버전
pub const MIN_VERSION: u8 = 2;
pub const HEADER_SIZE: u64 = 32;
pub const CODEC_ZSTD: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub version: u8,
    pub codec: u8,
    pub level: u8,
    pub entry_count: u64,
    pub total_size: u64,
    /// v3: 블록 인덱스가 시작하는 위치 (v2는 0)
    pub index_offset: u64,
}

impl Header {
    pub fn write_to<W: Write>(&self, w: &mut W) -> Result<()> {
        let mut buf = [0u8; HEADER_SIZE as usize];
        buf[0..4].copy_from_slice(&MAGIC);
        buf[4] = self.version;
        buf[5] = self.codec;
        buf[6] = self.level;
        buf[8..16].copy_from_slice(&self.entry_count.to_le_bytes());
        buf[16..24].copy_from_slice(&self.total_size.to_le_bytes());
        buf[24..32].copy_from_slice(&self.index_offset.to_le_bytes());
        w.write_all(&buf)?;
        Ok(())
    }

    pub fn read_from<R: Read>(r: &mut R) -> Result<Self> {
        let mut buf = [0u8; HEADER_SIZE as usize];
        r.read_exact(&mut buf)
            .map_err(|_| anyhow::anyhow!("bzx 파일이 아니거나 너무 짧습니다"))?;
        if buf[0..4] != MAGIC && buf[0..4] != LEGACY_MAGIC {
            bail!("bzx 파일이 아닙니다 (매직 넘버 불일치)");
        }
        if !(MIN_VERSION..=VERSION).contains(&buf[4]) {
            bail!(
                "지원하지 않는 포맷 버전: {} (이 프로그램은 버전 {VERSION})",
                buf[4]
            );
        }
        if buf[5] != CODEC_ZSTD {
            bail!("알 수 없는 코덱 ID: {}", buf[5]);
        }
        Ok(Header {
            version: buf[4],
            codec: buf[5],
            level: buf[6],
            entry_count: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            total_size: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
            index_offset: if buf[4] >= 3 {
                u64::from_le_bytes(buf[24..32].try_into().unwrap())
            } else {
                0
            },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
}

/// 항목 머리말 (파일이면 이 뒤에 데이터와 CRC32가 온다)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryHeader {
    pub kind: EntryKind,
    /// 아카이브 안 경로 ('/' 구분)
    pub path: String,
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
    /// 파일 크기 (폴더는 0)
    pub size: u64,
}

impl EntryHeader {
    pub fn write_to<W: Write>(&self, w: &mut W) -> Result<()> {
        let path = self.path.as_bytes();
        if path.len() > u16::MAX as usize {
            bail!("경로가 너무 깁니다: {}", self.path);
        }
        w.write_all(&[match self.kind {
            EntryKind::File => 1,
            EntryKind::Dir => 2,
        }])?;
        w.write_all(&(path.len() as u16).to_le_bytes())?;
        w.write_all(path)?;
        w.write_all(&self.mtime_secs.to_le_bytes())?;
        w.write_all(&self.mtime_nanos.to_le_bytes())?;
        if self.kind == EntryKind::File {
            w.write_all(&self.size.to_le_bytes())?;
        }
        Ok(())
    }

    /// 다음 항목을 읽는다. End 항목이면 `None`.
    pub fn read_from<R: Read>(r: &mut R) -> Result<Option<Self>> {
        let kind = match read_u8(r)? {
            0 => return Ok(None),
            1 => EntryKind::File,
            2 => EntryKind::Dir,
            other => bail!("알 수 없는 항목 종류 {other} (파일 손상)"),
        };
        let len = u16::from_le_bytes(read_array(r)?) as usize;
        let mut path = vec![0u8; len];
        r.read_exact(&mut path)?;
        let path = String::from_utf8(path)
            .map_err(|_| anyhow::anyhow!("경로가 UTF-8이 아닙니다 (파일 손상)"))?;
        let mtime_secs = i64::from_le_bytes(read_array(r)?);
        let mtime_nanos = u32::from_le_bytes(read_array(r)?);
        let size = if kind == EntryKind::File {
            u64::from_le_bytes(read_array(r)?)
        } else {
            0
        };
        Ok(Some(EntryHeader {
            kind,
            path,
            mtime_secs,
            mtime_nanos,
            size,
        }))
    }

    pub fn mtime(&self) -> SystemTime {
        let nanos = Duration::from_nanos(self.mtime_nanos as u64);
        if self.mtime_secs >= 0 {
            UNIX_EPOCH + Duration::from_secs(self.mtime_secs as u64) + nanos
        } else {
            UNIX_EPOCH - Duration::from_secs(self.mtime_secs.unsigned_abs()) + nanos
        }
    }
}

pub fn write_end<W: Write>(w: &mut W) -> Result<()> {
    w.write_all(&[0])?;
    Ok(())
}

/// SystemTime → (초, 나노초)
pub fn split_time(t: SystemTime) -> (i64, u32) {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            // 1970년 이전 — 초 단위로 내림
            let d = e.duration();
            let secs = -(d.as_secs() as i64) - if d.subsec_nanos() > 0 { 1 } else { 0 };
            let nanos = if d.subsec_nanos() > 0 {
                1_000_000_000 - d.subsec_nanos()
            } else {
                0
            };
            (secs, nanos)
        }
    }
}

/// 아카이브 안 경로를 안전한 상대 경로로 바꾼다.
/// `../`, 절대 경로, 드라이브 문자(`C:`) 등으로 압축 폴더 밖에 파일을 쓰는
/// 공격("Zip Slip")을 막는다.
pub fn safe_relative_path(archive_path: &str) -> Result<PathBuf> {
    if archive_path.is_empty() {
        bail!("빈 경로 (파일 손상)");
    }
    let mut out = PathBuf::new();
    for part in archive_path.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains(['\\', ':', '\0']) {
            bail!("안전하지 않은 경로라서 거부합니다: {archive_path}");
        }
        out.push(part);
    }
    // 이중 확인: 결과가 순수 상대 경로인지
    if !out.components().all(|c| matches!(c, Component::Normal(_))) {
        bail!("안전하지 않은 경로라서 거부합니다: {archive_path}");
    }
    Ok(out)
}

/// 다른 프로그램이 만든 압축 파일(zip, tar 등)의 경로를 Pouch 규칙으로 정리한다.
/// - 윈도우식 `\` → `/`
/// - 맨 앞 `/` 제거 (tar의 절대 경로는 GNU tar처럼 상대 경로로 바꿔 푼다)
/// - `./` 와 빈 조각 제거, 끝의 `/` 제거
///
/// - 윈도우에서 파일 이름에 쓸 수 없는 문자(`: * ? " < > |`, 제어 문자)는 `_`로 바꾼다
///   (맥·리눅스에서 만든 압축 파일에 흔함 — 반디집도 이렇게 바꿔서 푼다)
///
/// `..` 같은 위험한 조각은 그대로 남겨서 [`safe_relative_path`]가 거부하게 한다.
pub fn normalize_foreign_path(name: &str) -> String {
    name.replace('\\', "/")
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .map(|p| {
            p.chars()
                .map(|c| match c {
                    ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
                    c if c.is_control() => '_',
                    c => c,
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// 실제 경로(상대) → 아카이브 안 경로 ('/' 구분)
pub fn to_archive_path(rel: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str().ok_or_else(|| {
                anyhow::anyhow!("UTF-8로 바꿀 수 없는 파일 이름: {}", rel.display())
            })?),
            _ => bail!("예상치 못한 경로 형태: {}", rel.display()),
        }
    }
    Ok(parts.join("/"))
}

fn read_u8<R: Read>(r: &mut R) -> Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)
        .map_err(|_| anyhow::anyhow!("아카이브가 중간에 끊겼습니다 (파일 손상)"))?;
    Ok(b[0])
}

fn read_array<R: Read, const N: usize>(r: &mut R) -> Result<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b)
        .map_err(|_| anyhow::anyhow!("아카이브가 중간에 끊겼습니다 (파일 손상)"))?;
    Ok(b)
}
