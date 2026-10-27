//! 풀어낸 항목을 디스크에 쓰는 곳 — 모든 형식(bzx, zip, 7z, tar…)이 함께 쓴다.
//!
//! - 경로 검사 (Zip Slip 방어), 이름 충돌 처리(번호 붙이기/덮어쓰기/멈추기)
//! - 작은 파일(1MB 이하)은 쓰기 스레드 풀에 넘겨 동시에 쓴다.
//!   작은 파일이 수천 개인 폴더는 "파일 만들기" 자체가 병목이라(특히 Windows) 효과가 크다.
//! - 수정 시각 복원

use crate::format;
use crate::{Conflict, Control, HashingReader, Stats, unique_path};
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::SystemTime;

/// 이 크기 이하의 파일은 쓰기 스레드 풀로 보낸다
const SMALL_FILE: u64 = 1 << 20;

pub(crate) struct Sink<'a> {
    out_dir: Option<&'a Path>,
    conflict: Conflict,
    control: Option<Control>,
    /// 번호 붙이기 모드: 최상위 항목 이름 → 실제로 쓸 이름
    renames: HashMap<OsString, OsString>,
    stats: Stats,
    /// 폴더 수정 시각은 안에 파일을 다 쓴 뒤에 맞춰야 한다
    dir_times: Vec<(PathBuf, SystemTime)>,
    pool: Option<WriterPool>,
}

impl<'a> Sink<'a> {
    pub fn new(
        out_dir: Option<&'a Path>,
        conflict: Conflict,
        control: Option<Control>,
    ) -> Result<Self> {
        if let Some(dir) = out_dir {
            fs::create_dir_all(dir)
                .with_context(|| format!("출력 폴더를 만들 수 없음: {}", dir.display()))?;
        }
        Ok(Self {
            out_dir,
            conflict,
            control,
            renames: HashMap::new(),
            stats: Stats::default(),
            dir_times: Vec::new(),
            // 파일 만들기는 CPU보다 디스크·백신 검사를 기다리는 시간이 길어서 코어 수보다 넉넉히 둔다
            pool: out_dir.map(|_| WriterPool::new(crate::num_threads().clamp(4, 16) as usize)),
        })
    }

    /// 아카이브 안 경로 → 실제로 쓸 경로 (검증 모드면 None)
    fn target(&mut self, archive_path: &str) -> Result<Option<PathBuf>> {
        let mut rel = format::safe_relative_path(archive_path)?;
        let Some(dir) = self.out_dir else {
            return Ok(None);
        };
        // 최상위 항목(예: "사진" 폴더)이 이미 있으면 "사진 (1)"로 바꿔서, 그 안의 모든 항목을 새 폴더로 보낸다
        if self.conflict == Conflict::Rename {
            let mut parts = rel.components();
            let top = parts
                .next()
                .expect("빈 경로는 위에서 걸러짐")
                .as_os_str()
                .to_os_string();
            let rest: PathBuf = parts.collect();
            let stats = &mut self.stats;
            let new_top = self.renames.entry(top.clone()).or_insert_with(|| {
                let chosen = unique_path(&dir.join(&top));
                let new_name = chosen.file_name().unwrap_or(&top).to_os_string();
                if new_name != top {
                    stats.renamed.push((
                        top.to_string_lossy().into_owned(),
                        new_name.to_string_lossy().into_owned(),
                    ));
                }
                new_name
            });
            rel = if rest.as_os_str().is_empty() {
                PathBuf::from(&*new_top)
            } else {
                Path::new(new_top).join(rest)
            };
        }
        Ok(Some(dir.join(rel)))
    }

    pub fn dir(&mut self, archive_path: &str, mtime: Option<SystemTime>) -> Result<()> {
        if let Some(target) = self.target(archive_path)? {
            fs::create_dir_all(&target)
                .with_context(|| format!("폴더를 만들 수 없음: {}", target.display()))?;
            if let Some(t) = mtime {
                self.dir_times.push((target, t));
            }
        }
        self.stats.dirs += 1;
        Ok(())
    }

    /// 파일 하나를 쓴다. `data`는 이 파일의 내용만 읽히는 Reader여야 한다.
    /// (읽은 바이트 수, CRC32)를 돌려준다 — bzx는 이걸로 손상을 검사한다.
    pub fn file(
        &mut self,
        archive_path: &str,
        size_hint: u64,
        mtime: Option<SystemTime>,
        data: &mut dyn Read,
    ) -> Result<(u64, u32)> {
        let target = self.target(archive_path)?;
        let mut reader = HashingReader::new(data, self.control.clone());

        match target {
            None => {
                io::copy(&mut reader, &mut io::sink())?;
            }
            Some(target) => {
                if self.conflict != Conflict::Overwrite && target.exists() {
                    bail!(
                        "{} 이(가) 이미 있습니다. '덮어쓰기'나 '번호 붙이기'를 골라 다시 시도하세요.",
                        target.display()
                    );
                }
                let pool = self.pool.as_mut().expect("출력 폴더가 있으면 풀도 있음");
                if size_hint <= SMALL_FILE {
                    let mut buf = Vec::with_capacity(size_hint as usize);
                    reader.read_to_end(&mut buf)?;
                    pool.submit(target, buf, mtime)?;
                } else {
                    write_file(&target, &mut reader, mtime)?;
                }
            }
        }

        self.stats.files += 1;
        self.stats.total_size += reader.bytes;
        Ok((reader.bytes, reader.hasher.finalize()))
    }

    pub fn skip(&mut self, archive_path: &str) {
        self.stats.skipped.push(PathBuf::from(archive_path));
    }

    /// 쓰기 스레드를 모두 기다리고, 폴더 수정 시각을 맞춘 뒤 통계를 돌려준다
    pub fn finish(mut self) -> Result<Stats> {
        if let Some(pool) = self.pool.take() {
            pool.finish()?;
        }
        // 안쪽 폴더부터 시각을 맞춘다
        for (dir, time) in self.dir_times.iter().rev() {
            if let Ok(f) = File::open(dir) {
                f.set_modified(*time).ok(); // Windows에선 폴더를 이렇게 못 열 수 있음 — 무시
            }
        }
        Ok(std::mem::take(&mut self.stats))
    }
}

/// 큰 파일은 메모리에 모으지 않고 바로 흘려 쓴다
fn write_file(target: &Path, data: &mut dyn Read, mtime: Option<SystemTime>) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let f = File::create(target)
        .with_context(|| format!("파일을 만들 수 없음: {}", target.display()))?;
    let mut w = BufWriter::with_capacity(crate::BUF_SIZE, f);
    if let Err(e) = io::copy(data, &mut w) {
        drop(w);
        let _ = fs::remove_file(target); // 반쯤 쓴 파일은 지운다
        return Err(e.into());
    }
    let f = w.into_inner().map_err(|e| e.into_error())?;
    if let Some(t) = mtime {
        f.set_modified(t).ok(); // 시각 복원 실패는 치명적이지 않음
    }
    Ok(())
}

// ───────────────────────────── 쓰기 스레드 풀 ─────────────────────────────

struct WriteJob {
    path: PathBuf,
    data: Vec<u8>,
    mtime: Option<SystemTime>,
}

struct WriterPool {
    senders: Vec<SyncSender<WriteJob>>,
    handles: Vec<JoinHandle<()>>,
    error: Arc<Mutex<Option<anyhow::Error>>>,
}

impl WriterPool {
    fn new(n: usize) -> Self {
        let error = Arc::new(Mutex::new(None));
        let mut senders = Vec::new();
        let mut handles = Vec::new();
        for _ in 0..n {
            let (tx, rx) = sync_channel::<WriteJob>(64);
            let error = Arc::clone(&error);
            handles.push(std::thread::spawn(move || {
                let mut last_parent: Option<PathBuf> = None;
                for job in rx {
                    if let Err(e) = write_small(&job, &mut last_parent) {
                        error.lock().unwrap().get_or_insert(e);
                        return;
                    }
                }
            }));
            senders.push(tx);
        }
        Self {
            senders,
            handles,
            error,
        }
    }

    fn take_error(&self) -> Option<anyhow::Error> {
        self.error.lock().unwrap().take()
    }

    fn submit(&mut self, path: PathBuf, data: Vec<u8>, mtime: Option<SystemTime>) -> Result<()> {
        if let Some(e) = self.take_error() {
            return Err(e);
        }
        // 같은 폴더의 파일은 같은 스레드로 보낸다:
        // - 여러 스레드가 한 폴더에 동시에 파일을 만들 때 생기는 폴더 잠금 경쟁을 피하고
        // - 같은 경로가 두 번 나와도 순서대로 쓰인다
        let mut h = std::collections::hash_map::DefaultHasher::new();
        path.parent().hash(&mut h);
        let i = (h.finish() as usize) % self.senders.len();
        if self.senders[i]
            .send(WriteJob { path, data, mtime })
            .is_err()
        {
            return Err(self
                .take_error()
                .unwrap_or_else(|| anyhow::anyhow!("파일 쓰기 스레드가 멈췄습니다")));
        }
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.senders.clear();
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
        match self.take_error() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for WriterPool {
    fn drop(&mut self) {
        self.senders.clear();
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

fn write_small(job: &WriteJob, last_parent: &mut Option<PathBuf>) -> Result<()> {
    if let Some(parent) = job.path.parent()
        && last_parent.as_deref() != Some(parent)
    {
        fs::create_dir_all(parent)?;
        *last_parent = Some(parent.to_path_buf());
    }
    let mut f = File::create(&job.path)
        .with_context(|| format!("파일을 만들 수 없음: {}", job.path.display()))?;
    if let Err(e) = f.write_all(&job.data) {
        drop(f);
        let _ = fs::remove_file(&job.path);
        return Err(e.into());
    }
    if let Some(t) = job.mtime {
        f.set_modified(t).ok();
    }
    Ok(())
}
