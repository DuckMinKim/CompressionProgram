//! Pouch 압축 엔진
//!
//! - `.pouch` (자체 포맷): 블록 병렬 zstd — 압축·해제 모두 멀티코어
//! - `.zip` 만들기: 블록 병렬 Deflate — 어디서나 열리는 zip을 멀티코어로
//! - 풀기: pouch(.bzx 포함), zip, 7z, tar, tar.gz/bz2/xz/zst, 단일 .gz/.bz2/.xz/.zst

pub mod assoc;
mod block;
pub mod foreign;
pub mod format;
mod native;
mod sink;
mod zipwrite;

use anyhow::{Context, Result, bail};
use format::{EntryHeader, EntryKind};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;
use walkdir::WalkDir;

pub use foreign::ArchiveKind;

/// 입출력 버퍼 크기 (1 MiB)
pub(crate) const BUF_SIZE: usize = 1 << 20;

/// 진행률 보고와 취소를 위한 핸들. 복제해서 다른 스레드(GUI)와 공유한다.
#[derive(Clone, Default)]
pub struct Control {
    done: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
}

impl Control {
    /// 지금까지 처리한 원본 바이트 수
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// 만들 압축 파일 형식
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutFormat {
    /// 자체 포맷: 가장 빠르고 작다. Pouch로만 열린다.
    #[default]
    Pouch,
    /// zip: 윈도우 탐색기·반디집 등 어디서나 열린다.
    Zip,
}

impl OutFormat {
    pub fn extension(self) -> &'static str {
        match self {
            OutFormat::Pouch => "pouch",
            OutFormat::Zip => "zip",
        }
    }
}

pub struct CompressOptions {
    /// 압축 레벨 1~22 (기본 3). zip은 Deflate 레벨 1~9로 바꿔 쓴다.
    pub level: i32,
    /// 사용할 스레드 수
    pub threads: u32,
    /// 진행률·취소 (없어도 됨)
    pub control: Option<Control>,
    pub format: OutFormat,
}

impl Default for CompressOptions {
    fn default() -> Self {
        Self {
            level: 3,
            threads: num_threads(),
            control: None,
            format: OutFormat::Pouch,
        }
    }
}

pub fn num_threads() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}

/// 압축/해제 결과 통계
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub files: u64,
    pub dirs: u64,
    /// 원본 데이터 전체 크기
    pub total_size: u64,
    /// 건너뛴 항목 (심볼릭 링크 등)
    pub skipped: Vec<PathBuf>,
    /// 풀 때 이름이 겹쳐서 바꾼 항목 (원래 이름, 새 이름)
    pub renamed: Vec<(String, String)>,
}

/// 압축 파일 요약 정보 (목록 보기·진행률용)
#[derive(Debug, Clone)]
pub struct ArchiveInfo {
    pub kind: ArchiveKind,
    pub entry_count: u64,
    /// 풀었을 때의 전체 크기
    pub total_size: u64,
}

/// 풀 때 같은 이름의 파일·폴더가 이미 있으면 어떻게 할지
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Conflict {
    /// 뒤에 번호를 붙여 새 이름으로 푼다: `사진` → `사진 (1)`, `a.txt` → `a (1).txt`
    #[default]
    Rename,
    /// 기존 파일을 덮어쓴다 (폴더는 합쳐진다)
    Overwrite,
    /// 오류를 내고 멈춘다
    Stop,
}

// ───────────────────────────── 공통 도우미 ─────────────────────────────

/// 읽으면서 CRC32·바이트 수를 계산하고, 진행률 보고·취소 확인도 하는 Reader
pub(crate) struct HashingReader<R> {
    inner: R,
    pub hasher: crc32fast::Hasher,
    pub bytes: u64,
    control: Option<Control>,
}

impl<R: Read> HashingReader<R> {
    pub fn new(inner: R, control: Option<Control>) -> Self {
        Self {
            inner,
            hasher: crc32fast::Hasher::new(),
            bytes: 0,
            control,
        }
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(c) = &self.control
            && c.is_cancelled()
        {
            return Err(io::Error::other(Cancelled));
        }
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        if let Some(c) = &self.control {
            c.done.fetch_add(n as u64, Ordering::Relaxed);
        }
        Ok(n)
    }
}

/// 사용자가 취소했을 때의 오류
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("사용자가 취소했습니다")
    }
}
impl std::error::Error for Cancelled {}

/// 오류가 "취소"였는지 확인
pub fn is_cancelled_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.is::<Cancelled>()
            || c.downcast_ref::<io::Error>()
                .and_then(|io| io.get_ref())
                .is_some_and(|inner| inner.is::<Cancelled>())
    })
}

/// `path`가 이미 있으면 `이름 (1)`, `이름 (2)`… 중 비어 있는 경로를 돌려준다.
/// 파일은 확장자 앞에 번호를 붙인다: `a.txt` → `a (1).txt`
pub fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let parent = path.parent().unwrap_or(Path::new(""));
    let is_dir = path.is_dir();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    for n in 1.. {
        let candidate = match (&ext, is_dir) {
            (Some(ext), false) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
            _ => format!("{name} ({n})"),
        };
        let p = parent.join(candidate);
        if !p.exists() {
            return p;
        }
    }
    unreachable!()
}

/// 사람이 읽기 좋은 크기 문자열
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

// ───────────────────────────── 압축할 항목 모으기 ─────────────────────────────

/// 압축할 항목 하나 (폴더를 미리 훑어서 만든 목록)
pub(crate) struct InputEntry {
    /// 아카이브 안 경로 ('/' 구분)
    pub archive_path: String,
    pub kind: EntryKind,
    pub path: PathBuf,
    pub mtime: SystemTime,
}

/// 입력 파일·폴더를 훑어 압축할 항목 목록을 만든다.
/// 이름순으로 정렬해서 매번 같은 결과가 나오게 하고, 출력 파일 자신은 건너뛴다.
pub(crate) fn collect_inputs(
    inputs: &[PathBuf],
    output: &Path,
    stats: &mut Stats,
) -> Result<Vec<InputEntry>> {
    if inputs.is_empty() {
        bail!("압축할 항목이 없습니다");
    }
    let output_abs = dunce::canonicalize(output).unwrap_or_else(|_| output.to_path_buf());
    let mut names = HashSet::new();
    let mut entries = Vec::new();

    for input in inputs {
        let root_abs = dunce::canonicalize(input)
            .with_context(|| format!("입력을 찾을 수 없음: {}", input.display()))?;
        let root_name = root_abs.file_name().ok_or_else(|| {
            anyhow::anyhow!("드라이브 최상위는 압축할 수 없습니다: {}", input.display())
        })?;
        let root_name = format::to_archive_path(Path::new(root_name))?;
        // 이름이 겹치면 풀 때 섞이므로 거부
        if !names.insert(root_name.to_lowercase()) {
            bail!("이름이 같은 항목이 두 개 있습니다: {root_name}");
        }

        for entry in WalkDir::new(&root_abs)
            .follow_links(false)
            .sort_by_file_name()
        {
            let entry = entry?;
            let path = entry.path();
            let rel = path.strip_prefix(&root_abs)?;
            let archive_path = if rel.as_os_str().is_empty() {
                root_name.clone()
            } else {
                format!("{root_name}/{}", format::to_archive_path(rel)?)
            };
            let ft = entry.file_type();
            let kind = if ft.is_dir() {
                EntryKind::Dir
            } else if ft.is_file() {
                if path == output_abs {
                    continue;
                }
                EntryKind::File
            } else {
                // 심볼릭 링크 등은 아직 지원하지 않아 건너뛴다
                stats.skipped.push(path.to_path_buf());
                continue;
            };
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            entries.push(InputEntry {
                archive_path,
                kind,
                path: path.to_path_buf(),
                mtime,
            });
        }
    }
    Ok(entries)
}

/// 압축할 원본 전체 크기를 미리 센다 (진행률 표시용)
pub fn scan_total_size(inputs: &[PathBuf]) -> u64 {
    inputs
        .iter()
        .flat_map(|p| WalkDir::new(p).follow_links(false))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// 기본 출력 경로 (.pouch): 하나면 "사진" → "사진.pouch", 여러 개면 "<부모 폴더 이름>.pouch"
pub fn default_archive_path(inputs: &[PathBuf]) -> Result<PathBuf> {
    default_archive_path_for(inputs, OutFormat::Pouch)
}

/// 형식에 맞는 기본 출력 경로: "사진" → "사진.zip"
pub fn default_archive_path_for(inputs: &[PathBuf], format: OutFormat) -> Result<PathBuf> {
    let first = inputs
        .first()
        .ok_or_else(|| anyhow::anyhow!("압축할 항목이 없습니다"))?;
    let abs = dunce::canonicalize(first)?;
    let parent = abs.parent().unwrap_or(Path::new("."));
    let base = if inputs.len() == 1 {
        abs.file_name().map(|n| n.to_os_string())
    } else {
        parent.file_name().map(|n| n.to_os_string())
    };
    let mut name = base.unwrap_or_else(|| "archive".into());
    name.push(".");
    name.push(format.extension());
    Ok(parent.join(name))
}

// ───────────────────────────── 압축 ─────────────────────────────

/// 파일 또는 폴더 하나를 압축한다.
pub fn compress_path(input: &Path, output: &Path, opts: &CompressOptions) -> Result<Stats> {
    compress_paths(&[input.to_path_buf()], output, opts)
}

/// 여러 파일·폴더를 하나의 압축 파일로 만든다 (`opts.format`: pouch 또는 zip).
/// 실패하거나 취소되면 만들던 파일을 지운다.
pub fn compress_paths(inputs: &[PathBuf], output: &Path, opts: &CompressOptions) -> Result<Stats> {
    let result = match opts.format {
        OutFormat::Pouch => native::compress(inputs, output, opts),
        OutFormat::Zip => zipwrite::compress(inputs, output, opts),
    };
    if result.is_err() {
        let _ = fs::remove_file(output);
    }
    result
}

// ───────────────────────────── 해제 / 목록 ─────────────────────────────

/// 해제 중 항목마다 호출되는 콜백에 넘어가는 정보
pub struct EntryInfo<'a> {
    pub header: &'a EntryHeader,
}

/// 압축 파일을 풀어서 `out_dir` 아래에 만든다. 형식(pouch, zip, 7z, tar…)은 자동으로 알아낸다.
/// `out_dir`이 `None`이면 아무것도 쓰지 않고 검증만 한다 (테스트용).
pub fn extract_archive(
    input: &Path,
    out_dir: Option<&Path>,
    conflict: Conflict,
    on_entry: impl FnMut(EntryInfo),
) -> Result<(ArchiveInfo, Stats)> {
    extract_archive_with(input, out_dir, conflict, None, on_entry)
}

/// [`extract_archive`]에 진행률·취소 핸들을 더한 버전
pub fn extract_archive_with(
    input: &Path,
    out_dir: Option<&Path>,
    conflict: Conflict,
    control: Option<Control>,
    on_entry: impl FnMut(EntryInfo),
) -> Result<(ArchiveInfo, Stats)> {
    let kind = foreign::detect(input)?;
    let mut sink = sink::Sink::new(out_dir, conflict, control.clone())?;
    let info = match kind {
        ArchiveKind::Pouch => native::extract(input, &mut sink, control, on_entry)?,
        other => foreign::extract(input, other, &mut sink, on_entry)?,
    };
    let stats = sink.finish()?;
    Ok((info, stats))
}

/// 압축 파일 안의 항목 목록을 읽는다.
/// zip·7z는 목차만 읽어서 빠르고, pouch·tar 계열은 끝까지 읽으며 손상 여부도 검사한다.
pub fn list_archive(input: &Path) -> Result<(ArchiveInfo, Vec<EntryHeader>)> {
    let kind = foreign::detect(input)?;
    match kind {
        ArchiveKind::Zip | ArchiveKind::SevenZ => foreign::list_fast(input, kind),
        _ => {
            let mut entries = Vec::new();
            let (info, _) = extract_archive(input, None, Conflict::Stop, |e| {
                entries.push(e.header.clone())
            })?;
            Ok((info, entries))
        }
    }
}

/// Pouch 헤더 정보만 읽는다 (`info` 명령용). (헤더, 압축 데이터 크기)
pub fn read_header(input: &Path) -> Result<(format::Header, u64)> {
    let mut f = fs::File::open(input)?;
    let size = f.metadata()?.len();
    let header = format::Header::read_from(&mut f)?;
    Ok((header, size.saturating_sub(format::HEADER_SIZE)))
}
