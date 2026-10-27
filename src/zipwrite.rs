//! zip 만들기 — 멀티코어 Deflate
//!
//! 보통 zip 압축은 한 파일을 처음부터 끝까지 한 스레드로 압축한다.
//! 여기서는 파일을 1MB 조각으로 잘라 여러 스레드가 동시에 Deflate로 압축한다 (pigz와 같은 방식).
//! - 조각마다 앞 조각의 마지막 32KB를 "사전"으로 미리 넣어서 압축률 손해를 거의 없앤다.
//! - 마지막이 아닌 조각은 바이트 경계에서 끝나게(sync flush) 압축해서, 결과를 이어 붙이면
//!   하나의 올바른 Deflate 스트림이 된다. 그래서 어떤 zip 프로그램으로도 열린다.
//! - 압축이 안 되는 조각(이미 압축된 사진·영상의 대부분)은 조각마다 감지해서 압축 없이 넣는다.
//!   확장자로 판단하지 않는다 — 화면 녹화 mp4처럼 "압축된 형식"인데 잘 줄어드는 파일도 있다.
//!
//! zip 구조 (APPNOTE.TXT): [로컬 헤더 + 데이터] × N → [중앙 디렉터리] → [끝 레코드]
//! 4GB가 넘는 파일·위치·65535개 넘는 항목은 zip64 확장을 쓴다.

use crate::format::EntryKind;
use crate::{CompressOptions, HashingReader, Stats};
use anyhow::{Context, Result, bail};
use flate2::{Compress, Compression, FlushCompress, Status};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const CHUNK: usize = 1 << 20;
const DICT: usize = 32 * 1024;
/// 이보다 큰 파일은 zip64로 기록 (압축 후 조금 커질 여유를 둔 값)
const ZIP64_FILE: u64 = 0xF000_0000;
/// 데이터가 이보다 작으면 메모리에 모았다가 헤더와 함께 한 번에 쓴다 (되감기 없이)
const BUFFER_ENTRY: usize = 4 << 20;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Method {
    Store = 0,
    Deflate = 8,
}

/// 쓰기 스레드가 순서대로 처리할 조각
enum Piece {
    Dir {
        name: String,
        mtime: SystemTime,
    },
    FileStart {
        name: String,
        mtime: SystemTime,
        method: Method,
        zip64: bool,
    },
    Data(Vec<u8>),
    FileEnd {
        crc: u32,
        size: u64,
    },
}

struct Job {
    seq: u64,
    data: Vec<u8>,
    dict: Option<Vec<u8>>,
    last: bool,
    method: Method,
}

type Done = (u64, io::Result<Piece>);

/// Pouch 레벨(1~22) → Deflate 레벨(1~9)
fn deflate_level(level: i32) -> u32 {
    match level {
        ..=2 => 1,
        3..=5 => 5, // 반디집 기본값과 같은 레벨
        6..=12 => 7,
        _ => 9,
    }
}

pub(crate) fn compress(inputs: &[PathBuf], output: &Path, opts: &CompressOptions) -> Result<Stats> {
    let file = File::create(output)
        .with_context(|| format!("출력 파일을 만들 수 없음: {}", output.display()))?;
    let mut stats = Stats::default();
    let entries = crate::collect_inputs(inputs, output, &mut stats)?;
    let level = deflate_level(opts.level);
    let threads = opts.threads.max(1) as usize;

    let (job_tx, job_rx) = sync_channel::<Job>(threads * 2);
    let job_rx = Arc::new(Mutex::new(job_rx));
    let (done_tx, done_rx) = sync_channel::<Done>(threads * 4);

    let workers: Vec<_> = (0..threads)
        .map(|_| {
            let job_rx = Arc::clone(&job_rx);
            let done_tx = done_tx.clone();
            std::thread::spawn(move || deflate_worker(job_rx, done_tx, level))
        })
        .collect();
    let writer = std::thread::spawn(move || write_zip(file, done_rx));

    let produced = produce(&entries, &job_tx, &done_tx, opts, &mut stats);
    drop(job_tx);
    drop(done_tx);
    for w in workers {
        let _ = w.join();
    }
    let written = writer
        .join()
        .map_err(|_| anyhow::anyhow!("zip 쓰기 스레드가 비정상 종료했습니다"))?;
    match (produced, written) {
        (Ok(()), Ok(())) => Ok(stats),
        // 취소는 그대로 알리고, 그 밖엔 쓰기 쪽의 진짜 원인(디스크 가득 참 등)을 보여 준다
        (Err(p), Err(w)) => Err(if crate::is_cancelled_error(&p) { p } else { w }),
        (Err(p), Ok(())) => Err(p),
        (Ok(()), Err(w)) => Err(w),
    }
}

/// 메인 스레드: 파일을 순서대로 읽어 조각으로 나눠 보낸다
fn produce(
    entries: &[crate::InputEntry],
    jobs: &SyncSender<Job>,
    done: &SyncSender<Done>,
    opts: &CompressOptions,
    stats: &mut Stats,
) -> Result<()> {
    let mut seq = 0u64;
    let mut next_seq = || {
        seq += 1;
        seq - 1
    };
    let stopped = || anyhow::anyhow!("zip 쓰기가 중단되었습니다");

    for e in entries {
        match e.kind {
            EntryKind::Dir => {
                let piece = Piece::Dir {
                    name: format!("{}/", e.archive_path),
                    mtime: e.mtime,
                };
                done.send((next_seq(), Ok(piece))).map_err(|_| stopped())?;
                stats.dirs += 1;
            }
            EntryKind::File => {
                let src = File::open(&e.path)
                    .with_context(|| format!("파일을 열 수 없음: {}", e.path.display()))?;
                let size = src.metadata()?.len();
                let mut reader = HashingReader::new(
                    BufReader::with_capacity(crate::BUF_SIZE, src).take(size),
                    opts.control.clone(),
                );

                let mut chunk = read_chunk(&mut reader)?;
                // 빈 파일만 "저장". 나머지는 Deflate로 하되, 압축이 안 되는 조각은
                // 작업 스레드가 조각마다 판단해서 압축 없이(레벨 0) 넣는다
                let method = if size == 0 {
                    Method::Store
                } else {
                    Method::Deflate
                };
                let start = Piece::FileStart {
                    name: e.archive_path.clone(),
                    mtime: e.mtime,
                    method,
                    zip64: size >= ZIP64_FILE,
                };
                done.send((next_seq(), Ok(start))).map_err(|_| stopped())?;

                let mut dict: Option<Vec<u8>> = None;
                loop {
                    let last = reader.bytes == size || chunk.len() < CHUNK;
                    let tail = (method == Method::Deflate && !last)
                        .then(|| chunk[chunk.len().saturating_sub(DICT)..].to_vec());
                    let job = Job {
                        seq: next_seq(),
                        data: chunk,
                        dict: dict.take(),
                        last,
                        method,
                    };
                    jobs.send(job).map_err(|_| stopped())?;
                    if last {
                        break;
                    }
                    dict = tail;
                    chunk = read_chunk(&mut reader)?;
                }
                if reader.bytes != size {
                    bail!("압축 중에 파일 크기가 바뀌었습니다: {}", e.path.display());
                }
                let end = Piece::FileEnd {
                    crc: reader.hasher.finalize(),
                    size,
                };
                done.send((next_seq(), Ok(end))).map_err(|_| stopped())?;
                stats.files += 1;
                stats.total_size += size;
            }
        }
    }
    Ok(())
}

fn read_chunk(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; CHUNK];
    let mut n = 0;
    while n < CHUNK {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    buf.truncate(n);
    Ok(buf)
}

fn deflate_worker(jobs: Arc<Mutex<Receiver<Job>>>, done: SyncSender<Done>, level: u32) {
    loop {
        let job = jobs.lock().ok().and_then(|rx| rx.recv().ok());
        let Some(job) = job else { return };
        let seq = job.seq;
        let result = match job.method {
            Method::Store => Ok(Piece::Data(job.data)),
            Method::Deflate => deflate_chunk(&job, level).map(Piece::Data),
        };
        if done.send((seq, result)).is_err() {
            return;
        }
    }
}

/// 조각 하나를 raw Deflate로 압축한다.
/// 마지막 조각은 스트림을 끝내고(Finish), 나머지는 바이트 경계에서 끊는다(Sync).
fn deflate_chunk(job: &Job, level: u32) -> io::Result<Vec<u8>> {
    // 압축이 안 되는 조각은 레벨 0(압축 없이 Deflate의 "저장 블록"으로)으로 빠르게 넘긴다
    let level = if crate::block::looks_incompressible(&job.data) {
        0
    } else {
        level
    };
    let mut c = Compress::new(Compression::new(level), false);
    if let Some(d) = &job.dict {
        c.set_dictionary(d).map_err(io::Error::other)?;
    }
    let flush = if job.last {
        FlushCompress::Finish
    } else {
        FlushCompress::Sync
    };
    let mut out = Vec::with_capacity(job.data.len() / 2 + 1024);
    let mut pos = 0usize;
    loop {
        if out.capacity() - out.len() < 64 * 1024 {
            out.reserve(256 * 1024);
        }
        let before = c.total_in();
        let status = c
            .compress_vec(&job.data[pos..], &mut out, flush)
            .map_err(io::Error::other)?;
        pos += (c.total_in() - before) as usize;
        let finished = if job.last {
            status == Status::StreamEnd
        } else {
            // 입력을 다 넣었고 출력 공간이 남았다 = flush가 끝까지 됐다
            pos == job.data.len() && out.len() < out.capacity()
        };
        if finished {
            return Ok(out);
        }
    }
}

// ───────────────────────────── zip 파일 쓰기 ─────────────────────────────

struct Central {
    name: Vec<u8>,
    method: Method,
    dos_time: u16,
    dos_date: u16,
    unix_time: Option<u32>,
    crc: u32,
    comp: u64,
    size: u64,
    offset: u64,
    zip64: bool,
    is_dir: bool,
}

struct Current {
    central: Central,
    /// 아직 헤더를 안 썼다면 데이터를 여기 모은다
    buffered: Option<Vec<u8>>,
    comp: u64,
}

struct ZipOut {
    w: BufWriter<File>,
    pos: u64,
    central: Vec<Central>,
}

impl ZipOut {
    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.w.write_all(bytes)?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    fn local_header(&mut self, c: &Central) -> io::Result<()> {
        let mut h = Vec::with_capacity(30 + c.name.len() + 30);
        h.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        h.extend_from_slice(&version_needed(c).to_le_bytes());
        h.extend_from_slice(&0x0800u16.to_le_bytes()); // 이름이 UTF-8
        h.extend_from_slice(&(c.method as u16).to_le_bytes());
        h.extend_from_slice(&c.dos_time.to_le_bytes());
        h.extend_from_slice(&c.dos_date.to_le_bytes());
        h.extend_from_slice(&c.crc.to_le_bytes());
        if c.zip64 {
            h.extend_from_slice(&u32::MAX.to_le_bytes());
            h.extend_from_slice(&u32::MAX.to_le_bytes());
        } else {
            h.extend_from_slice(&(c.comp as u32).to_le_bytes());
            h.extend_from_slice(&(c.size as u32).to_le_bytes());
        }
        let extra = local_extra(c);
        h.extend_from_slice(&(c.name.len() as u16).to_le_bytes());
        h.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        h.extend_from_slice(&c.name);
        h.extend_from_slice(&extra);
        self.put(&h)
    }
}

fn version_needed(c: &Central) -> u16 {
    if c.zip64 {
        45
    } else if c.method == Method::Deflate || c.is_dir {
        20
    } else {
        10
    }
}

fn local_extra(c: &Central) -> Vec<u8> {
    let mut x = Vec::new();
    if c.zip64 {
        x.extend_from_slice(&0x0001u16.to_le_bytes());
        x.extend_from_slice(&16u16.to_le_bytes());
        x.extend_from_slice(&c.size.to_le_bytes());
        x.extend_from_slice(&c.comp.to_le_bytes());
    }
    if let Some(t) = c.unix_time {
        // 확장 타임스탬프: 시간대와 상관없는 정확한 UTC 수정 시각
        x.extend_from_slice(&0x5455u16.to_le_bytes());
        x.extend_from_slice(&5u16.to_le_bytes());
        x.push(1);
        x.extend_from_slice(&t.to_le_bytes());
    }
    x
}

/// 로컬 헤더 안에서 CRC가 있는 위치, zip64 크기 필드가 있는 위치
fn patch_offsets(c: &Central) -> (u64, Option<u64>) {
    let crc_at = c.offset + 14;
    let zip64_at = c.zip64.then(|| c.offset + 30 + c.name.len() as u64 + 4);
    (crc_at, zip64_at)
}

fn write_zip(file: File, done: Receiver<Done>) -> Result<()> {
    let mut out = ZipOut {
        w: BufWriter::with_capacity(crate::BUF_SIZE, file),
        pos: 0,
        central: Vec::new(),
    };
    let mut pending: BTreeMap<u64, Piece> = BTreeMap::new();
    let mut next = 0u64;
    let mut cur: Option<Current> = None;

    for (seq, piece) in done {
        pending.insert(seq, piece?);
        while let Some(piece) = pending.remove(&next) {
            next += 1;
            handle_piece(&mut out, &mut cur, piece)?;
        }
    }
    if !pending.is_empty() || cur.is_some() {
        // 메인 스레드가 중간에 멈춤 (취소·읽기 오류) — 호출한 쪽이 파일을 지운다
        return Ok(());
    }
    finish_zip(&mut out)?;
    out.w.flush()?;
    out.w.get_ref().sync_all()?;
    Ok(())
}

fn handle_piece(out: &mut ZipOut, cur: &mut Option<Current>, piece: Piece) -> Result<()> {
    match piece {
        Piece::Dir { name, mtime } => {
            let (dos_time, dos_date, unix_time) = times(mtime);
            let c = Central {
                name: name.into_bytes(),
                method: Method::Store,
                dos_time,
                dos_date,
                unix_time,
                crc: 0,
                comp: 0,
                size: 0,
                offset: out.pos,
                zip64: false,
                is_dir: true,
            };
            out.local_header(&c)?;
            out.central.push(c);
        }
        Piece::FileStart {
            name,
            mtime,
            method,
            zip64,
        } => {
            let (dos_time, dos_date, unix_time) = times(mtime);
            *cur = Some(Current {
                central: Central {
                    name: name.into_bytes(),
                    method,
                    dos_time,
                    dos_date,
                    unix_time,
                    crc: 0,
                    comp: 0,
                    size: 0,
                    offset: 0,
                    zip64,
                    is_dir: false,
                },
                buffered: Some(Vec::new()),
                comp: 0,
            });
        }
        Piece::Data(bytes) => {
            let c = cur.as_mut().context("zip 조각 순서 오류")?;
            c.comp += bytes.len() as u64;
            match &mut c.buffered {
                Some(buf) if buf.len() + bytes.len() <= BUFFER_ENTRY => {
                    buf.extend_from_slice(&bytes)
                }
                Some(_) => {
                    // 커서 메모리에 다 모을 수 없음 → 헤더를 먼저 쓰고 흘려 쓴다 (끝나면 되돌아가 고침)
                    let buf = c.buffered.take().unwrap();
                    c.central.offset = out.pos;
                    out.local_header(&c.central)?;
                    out.put(&buf)?;
                    out.put(&bytes)?;
                }
                None => out.put(&bytes)?,
            }
        }
        Piece::FileEnd { crc, size } => {
            let mut c = cur.take().context("zip 조각 순서 오류")?;
            c.central.crc = crc;
            c.central.size = size;
            c.central.comp = c.comp;
            if !c.central.zip64 && c.comp >= u32::MAX as u64 {
                bail!("압축된 크기가 예상보다 커서 zip에 담을 수 없습니다");
            }
            match c.buffered.take() {
                Some(buf) => {
                    c.central.offset = out.pos;
                    out.local_header(&c.central)?;
                    out.put(&buf)?;
                }
                None => {
                    // 헤더로 되돌아가 CRC·크기를 채운다
                    let end = out.pos;
                    let (crc_at, zip64_at) = patch_offsets(&c.central);
                    out.w.seek(SeekFrom::Start(crc_at))?;
                    out.w.write_all(&crc.to_le_bytes())?;
                    if let Some(at) = zip64_at {
                        out.w.seek(SeekFrom::Start(at))?;
                        out.w.write_all(&size.to_le_bytes())?;
                        out.w.write_all(&c.comp.to_le_bytes())?;
                    } else {
                        out.w.write_all(&(c.comp as u32).to_le_bytes())?;
                        out.w.write_all(&(size as u32).to_le_bytes())?;
                    }
                    out.w.seek(SeekFrom::Start(end))?;
                }
            }
            out.central.push(c.central);
        }
    }
    Ok(())
}

fn finish_zip(out: &mut ZipOut) -> io::Result<()> {
    let cd_start = out.pos;
    let central = std::mem::take(&mut out.central);
    for c in &central {
        let need_offset64 = c.offset >= u32::MAX as u64;
        let mut extra = Vec::new();
        if c.zip64 || need_offset64 {
            let mut fields = Vec::new();
            if c.zip64 {
                fields.extend_from_slice(&c.size.to_le_bytes());
                fields.extend_from_slice(&c.comp.to_le_bytes());
            }
            if need_offset64 {
                fields.extend_from_slice(&c.offset.to_le_bytes());
            }
            extra.extend_from_slice(&0x0001u16.to_le_bytes());
            extra.extend_from_slice(&(fields.len() as u16).to_le_bytes());
            extra.extend_from_slice(&fields);
        }
        if let Some(t) = c.unix_time {
            extra.extend_from_slice(&0x5455u16.to_le_bytes());
            extra.extend_from_slice(&5u16.to_le_bytes());
            extra.push(1);
            extra.extend_from_slice(&t.to_le_bytes());
        }

        let mut h = Vec::with_capacity(46 + c.name.len() + extra.len());
        h.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        h.extend_from_slice(&45u16.to_le_bytes()); // 만든 버전 4.5, MS-DOS 계열
        h.extend_from_slice(&version_needed(c).to_le_bytes());
        h.extend_from_slice(&0x0800u16.to_le_bytes());
        h.extend_from_slice(&(c.method as u16).to_le_bytes());
        h.extend_from_slice(&c.dos_time.to_le_bytes());
        h.extend_from_slice(&c.dos_date.to_le_bytes());
        h.extend_from_slice(&c.crc.to_le_bytes());
        if c.zip64 {
            h.extend_from_slice(&u32::MAX.to_le_bytes());
            h.extend_from_slice(&u32::MAX.to_le_bytes());
        } else {
            h.extend_from_slice(&(c.comp as u32).to_le_bytes());
            h.extend_from_slice(&(c.size as u32).to_le_bytes());
        }
        h.extend_from_slice(&(c.name.len() as u16).to_le_bytes());
        h.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        h.extend_from_slice(&0u16.to_le_bytes()); // 주석 길이
        h.extend_from_slice(&0u16.to_le_bytes()); // 디스크 번호
        h.extend_from_slice(&0u16.to_le_bytes()); // 내부 속성
        let attr: u32 = if c.is_dir { 0x10 } else { 0x20 }; // DOS: 폴더 / 보관
        h.extend_from_slice(&attr.to_le_bytes());
        let off32 = if need_offset64 {
            u32::MAX
        } else {
            c.offset as u32
        };
        h.extend_from_slice(&off32.to_le_bytes());
        h.extend_from_slice(&c.name);
        h.extend_from_slice(&extra);
        out.put(&h)?;
    }
    let cd_size = out.pos - cd_start;
    let count = central.len() as u64;

    let need64 = count >= 0xFFFF || cd_size >= u32::MAX as u64 || cd_start >= u32::MAX as u64;
    if need64 {
        let z64_eocd = out.pos;
        let mut r = Vec::new();
        r.extend_from_slice(&0x0606_4b50u32.to_le_bytes());
        r.extend_from_slice(&44u64.to_le_bytes());
        r.extend_from_slice(&45u16.to_le_bytes());
        r.extend_from_slice(&45u16.to_le_bytes());
        r.extend_from_slice(&0u32.to_le_bytes());
        r.extend_from_slice(&0u32.to_le_bytes());
        r.extend_from_slice(&count.to_le_bytes());
        r.extend_from_slice(&count.to_le_bytes());
        r.extend_from_slice(&cd_size.to_le_bytes());
        r.extend_from_slice(&cd_start.to_le_bytes());
        // zip64 끝 레코드 위치 표시
        r.extend_from_slice(&0x0706_4b50u32.to_le_bytes());
        r.extend_from_slice(&0u32.to_le_bytes());
        r.extend_from_slice(&z64_eocd.to_le_bytes());
        r.extend_from_slice(&1u32.to_le_bytes());
        out.put(&r)?;
    }
    let mut e = Vec::new();
    e.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    e.extend_from_slice(&0u16.to_le_bytes());
    e.extend_from_slice(&0u16.to_le_bytes());
    let c16 = count.min(0xFFFF) as u16;
    e.extend_from_slice(&c16.to_le_bytes());
    e.extend_from_slice(&c16.to_le_bytes());
    e.extend_from_slice(&(cd_size.min(u32::MAX as u64) as u32).to_le_bytes());
    e.extend_from_slice(&(cd_start.min(u32::MAX as u64) as u32).to_le_bytes());
    e.extend_from_slice(&0u16.to_le_bytes());
    out.put(&e)
}

/// (DOS 시각, DOS 날짜, UNIX 시각)
fn times(t: SystemTime) -> (u16, u16, Option<u32>) {
    use chrono::{Datelike, TimeZone, Timelike};
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let unix = u32::try_from(secs).ok();
    // DOS 시각은 현지 시간, 1980~2107년, 2초 단위
    let local = chrono::Local.timestamp_opt(secs, 0).single();
    let (time, date) = match local {
        Some(l) if (1980..=2107).contains(&l.year()) => (
            ((l.hour() << 11) | (l.minute() << 5) | (l.second() / 2)) as u16,
            ((((l.year() - 1980) as u32) << 9) | (l.month() << 5) | l.day()) as u16,
        ),
        _ => (0, (1 << 5) | 1), // 1980-01-01
    };
    (time, date, unix)
}
