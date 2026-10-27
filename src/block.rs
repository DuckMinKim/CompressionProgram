//! 블록 병렬 압축/해제 — Pouch 포맷(v3)의 핵심
//!
//! 압축: 항목 스트림을 4MB 블록으로 잘라 작업 스레드들이 동시에 압축하고,
//!       출력 스레드가 원래 순서대로 이어 붙인다.
//! 해제: 블록마다 독립적이므로 여러 블록을 동시에 풀고, 순서대로 읽어 준다.
//!
//! ```text
//!            ┌─ 작업자 1 ─┐
//! 메인 ──블록──┼─ 작업자 2 ─┼──결과──▶ 출력(순서 맞추기) ──▶ 파일
//!            └─ 작업자 N ─┘
//! ```

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// 블록 하나의 원본 크기
pub const BLOCK_SIZE: usize = 4 << 20;

/// 블록 인덱스 항목
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockMeta {
    pub comp_size: u32,
    pub raw_size: u32,
    /// 압축하지 않고 그대로 저장했는지
    pub stored: bool,
}

type Job = (u64, Vec<u8>);
/// 압축이 끝난 파일과 블록 인덱스
type Written = (File, Vec<BlockMeta>);
type Done = (u64, io::Result<(Vec<u8>, BlockMeta)>);

// ───────────────────────────── 압축 ─────────────────────────────

/// 쓰는 데이터를 블록으로 모아 병렬 압축하는 Writer
pub struct BlockWriter {
    buf: Vec<u8>,
    seq: u64,
    jobs: Option<SyncSender<Job>>,
    workers: Vec<JoinHandle<()>>,
    output: Option<JoinHandle<io::Result<Written>>>,
}

impl BlockWriter {
    /// `file`은 블록을 쓰기 시작할 위치에 있어야 한다.
    pub fn new(file: File, level: i32, threads: u32) -> Self {
        let threads = threads.max(1) as usize;
        let (job_tx, job_rx) = sync_channel::<Job>(threads * 2);
        let job_rx = Arc::new(Mutex::new(job_rx));
        // 결과 채널도 크기를 제한해서, 디스크가 느릴 때 메모리가 끝없이 늘지 않게 한다
        let (done_tx, done_rx) = sync_channel::<Done>(threads * 2);

        let workers = (0..threads)
            .map(|_| {
                let job_rx = Arc::clone(&job_rx);
                let done_tx = done_tx.clone();
                std::thread::spawn(move || compress_worker(job_rx, done_tx, level))
            })
            .collect();
        drop(done_tx);

        let output = std::thread::spawn(move || write_in_order(file, done_rx));

        Self {
            buf: Vec::with_capacity(BLOCK_SIZE),
            seq: 0,
            jobs: Some(job_tx),
            workers,
            output: Some(output),
        }
    }

    fn send_block(&mut self) -> io::Result<()> {
        let block = std::mem::replace(&mut self.buf, Vec::with_capacity(BLOCK_SIZE));
        let tx = self.jobs.as_ref().expect("finish 뒤에는 쓸 수 없음");
        tx.send((self.seq, block))
            .map_err(|_| io::Error::other("압축 작업 스레드가 멈췄습니다"))?;
        self.seq += 1;
        Ok(())
    }

    /// 남은 데이터를 보내고 모든 스레드가 끝나길 기다린다. (파일, 블록 인덱스)를 돌려준다.
    pub fn finish(mut self) -> io::Result<(File, Vec<BlockMeta>)> {
        let sent = if self.buf.is_empty() {
            Ok(())
        } else {
            self.send_block()
        };
        drop(self.jobs.take());
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        let out = self
            .output
            .take()
            .expect("출력 스레드")
            .join()
            .map_err(|_| io::Error::other("출력 스레드가 비정상 종료했습니다"))?;
        // 실제 원인(디스크 가득 참 등)은 출력 스레드 쪽 오류이므로 그걸 먼저 돌려준다
        let out = out?;
        sent?;
        Ok(out)
    }
}

impl Write for BlockWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let room = BLOCK_SIZE - self.buf.len();
        let n = room.min(data.len());
        self.buf.extend_from_slice(&data[..n]);
        if self.buf.len() == BLOCK_SIZE {
            self.send_block()?;
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for BlockWriter {
    fn drop(&mut self) {
        // finish 없이 버려진 경우(오류·취소): 채널을 닫아 스레드들이 끝나게 한다
        drop(self.jobs.take());
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        if let Some(o) = self.output.take() {
            let _ = o.join();
        }
    }
}

fn compress_worker(jobs: Arc<Mutex<Receiver<Job>>>, done: SyncSender<Done>, level: i32) {
    let mut compressor = match zstd::bulk::Compressor::new(level) {
        Ok(c) => c,
        Err(e) => {
            let _ = done.send((0, Err(e)));
            return;
        }
    };
    loop {
        // 잠금은 다음 일감을 꺼내는 동안만 잡는다
        let job = jobs.lock().ok().and_then(|rx| rx.recv().ok());
        let Some((seq, raw)) = job else { return };
        let result = compress_block(&mut compressor, raw);
        if done.send((seq, result)).is_err() {
            return; // 출력 스레드가 오류로 끝남
        }
    }
}

fn compress_block(
    compressor: &mut zstd::bulk::Compressor,
    raw: Vec<u8>,
) -> io::Result<(Vec<u8>, BlockMeta)> {
    let raw_size = raw.len() as u32;
    let stored = |raw: Vec<u8>| {
        let meta = BlockMeta {
            comp_size: raw.len() as u32,
            raw_size,
            stored: true,
        };
        Ok((raw, meta))
    };
    // 사진·영상·zip처럼 이미 압축된 데이터는 압축해 봐야 시간만 든다
    if looks_incompressible(&raw) {
        return stored(raw);
    }
    let comp = compressor.compress(&raw)?;
    if comp.len() >= raw.len() {
        return stored(raw);
    }
    let meta = BlockMeta {
        comp_size: comp.len() as u32,
        raw_size,
        stored: false,
    };
    Ok((comp, meta))
}

/// 블록이 이미 압축된 데이터처럼 보이는지 빠르게 추정한다.
///
/// 바이트 분포의 엔트로피가 8비트에 가깝고, 앞부분 64KB를 가장 빠른 레벨로 압축해도
/// 3% 넘게 줄지 않으면 "압축 불가"로 본다.
pub fn looks_incompressible(data: &[u8]) -> bool {
    if data.len() < 256 * 1024 {
        return false;
    }
    let mut hist = [0u32; 256];
    let mut n = 0u32;
    for &b in data.iter().step_by(7) {
        hist[b as usize] += 1;
        n += 1;
    }
    let n = n as f64;
    let entropy: f64 = hist
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum();
    if entropy < 7.9 {
        return false;
    }
    let sample = &data[..64 * 1024];
    match zstd::bulk::compress(sample, 1) {
        Ok(c) => c.len() as f64 > sample.len() as f64 * 0.97,
        Err(_) => false,
    }
}

/// 작업자들이 보내는 결과를 순서대로 맞춰 파일에 쓴다
fn write_in_order(file: File, done: Receiver<Done>) -> io::Result<(File, Vec<BlockMeta>)> {
    let mut out = BufWriter::with_capacity(crate::BUF_SIZE, file);
    let mut pending: BTreeMap<u64, (Vec<u8>, BlockMeta)> = BTreeMap::new();
    let mut next = 0u64;
    let mut metas = Vec::new();
    for (seq, result) in done {
        let block = result?;
        pending.insert(seq, block);
        while let Some((data, meta)) = pending.remove(&next) {
            out.write_all(&data)?;
            metas.push(meta);
            next += 1;
        }
    }
    if !pending.is_empty() {
        return Err(io::Error::other("압축 블록이 빠졌습니다"));
    }
    out.flush()?;
    let file = out.into_inner().map_err(|e| e.into_error())?;
    Ok((file, metas))
}

// ───────────────────────────── 해제 ─────────────────────────────

type DecJob = (u64, Vec<u8>, BlockMeta);
type DecDone = (u64, io::Result<Vec<u8>>);

/// 블록들을 여러 스레드로 동시에 풀고, 원래 순서대로 읽어 주는 Reader
pub struct BlockReader {
    done: Receiver<DecDone>,
    pending: BTreeMap<u64, io::Result<Vec<u8>>>,
    next: u64,
    total: u64,
    cur: Vec<u8>,
    pos: usize,
}

impl BlockReader {
    /// `file`은 첫 블록 위치에 있어야 한다.
    pub fn new(file: File, metas: Vec<BlockMeta>, threads: u32) -> Self {
        let threads = threads.max(1) as usize;
        let total = metas.len() as u64;
        let (job_tx, job_rx) = sync_channel::<DecJob>(threads * 2);
        let job_rx = Arc::new(Mutex::new(job_rx));
        let (done_tx, done_rx) = sync_channel::<DecDone>(threads * 2);

        // 읽기 스레드: 압축된 블록을 순서대로 읽어 작업자에게 나눠 준다
        {
            let done_tx = done_tx.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::with_capacity(crate::BUF_SIZE, file);
                for (seq, meta) in metas.into_iter().enumerate() {
                    let mut comp = vec![0u8; meta.comp_size as usize];
                    if let Err(e) = r.read_exact(&mut comp) {
                        let _ = done_tx.send((seq as u64, Err(e)));
                        return;
                    }
                    if job_tx.send((seq as u64, comp, meta)).is_err() {
                        return; // 읽는 쪽이 그만둠 (취소·오류)
                    }
                }
            });
        }
        for _ in 0..threads {
            let job_rx = Arc::clone(&job_rx);
            let done_tx = done_tx.clone();
            std::thread::spawn(move || {
                let mut dec = match zstd::bulk::Decompressor::new() {
                    Ok(d) => d,
                    Err(e) => {
                        let _ = done_tx.send((0, Err(e)));
                        return;
                    }
                };
                loop {
                    let job = job_rx.lock().ok().and_then(|rx| rx.recv().ok());
                    let Some((seq, comp, meta)) = job else { return };
                    let result = if meta.stored {
                        Ok(comp)
                    } else {
                        dec.decompress(&comp, meta.raw_size as usize)
                            .and_then(|raw| {
                                if raw.len() == meta.raw_size as usize {
                                    Ok(raw)
                                } else {
                                    Err(io::Error::other("블록 크기가 맞지 않습니다 (파일 손상)"))
                                }
                            })
                    };
                    if done_tx.send((seq, result)).is_err() {
                        return;
                    }
                }
            });
        }

        Self {
            done: done_rx,
            pending: BTreeMap::new(),
            next: 0,
            total,
            cur: Vec::new(),
            pos: 0,
        }
    }

    fn next_block(&mut self) -> io::Result<bool> {
        if self.next == self.total {
            return Ok(false);
        }
        let block = loop {
            if let Some(b) = self.pending.remove(&self.next) {
                break b;
            }
            match self.done.recv() {
                Ok((seq, b)) => {
                    self.pending.insert(seq, b);
                }
                Err(_) => return Err(io::Error::other("블록을 읽지 못했습니다 (파일 손상)")),
            }
        };
        self.cur = block
            .map_err(|e| io::Error::new(e.kind(), format!("블록 해제 실패 (파일 손상): {e}")))?;
        self.pos = 0;
        self.next += 1;
        Ok(true)
    }
}

impl Read for BlockReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.cur.len() {
            if !self.next_block()? {
                return Ok(0);
            }
        }
        let n = buf.len().min(self.cur.len() - self.pos);
        buf[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

// ───────────────────────────── 인덱스 ─────────────────────────────

const INDEX_MAGIC: [u8; 4] = *b"PCHI";
/// 예전 이름(bzx)으로 만든 파일의 인덱스 표시
const LEGACY_INDEX_MAGIC: [u8; 4] = *b"BZXI";

pub fn write_index<W: Write>(w: &mut W, metas: &[BlockMeta]) -> io::Result<()> {
    w.write_all(&INDEX_MAGIC)?;
    w.write_all(&(metas.len() as u32).to_le_bytes())?;
    for m in metas {
        w.write_all(&m.comp_size.to_le_bytes())?;
        w.write_all(&m.raw_size.to_le_bytes())?;
        w.write_all(&[m.stored as u8])?;
    }
    Ok(())
}

pub fn read_index<R: Read>(r: &mut R) -> io::Result<Vec<BlockMeta>> {
    let bad = || io::Error::other("블록 인덱스가 손상되었습니다");
    let mut head = [0u8; 8];
    r.read_exact(&mut head).map_err(|_| bad())?;
    if head[0..4] != INDEX_MAGIC && head[0..4] != LEGACY_INDEX_MAGIC {
        return Err(bad());
    }
    let count = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
    let mut metas = Vec::with_capacity(count.min(1 << 20));
    for _ in 0..count {
        let mut e = [0u8; 9];
        r.read_exact(&mut e).map_err(|_| bad())?;
        let meta = BlockMeta {
            comp_size: u32::from_le_bytes(e[0..4].try_into().unwrap()),
            raw_size: u32::from_le_bytes(e[4..8].try_into().unwrap()),
            stored: e[8] == 1,
        };
        // 손상·악성 파일이 터무니없는 크기로 메모리를 잡아먹지 못하게 상한을 둔다
        let max_comp = zstd::zstd_safe::compress_bound(BLOCK_SIZE);
        if meta.raw_size as usize > BLOCK_SIZE
            || meta.comp_size as usize > max_comp
            || (meta.stored && meta.comp_size != meta.raw_size)
        {
            return Err(bad());
        }
        metas.push(meta);
    }
    Ok(metas)
}
