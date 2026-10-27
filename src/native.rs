//! .bzx (Pouch 자체 형식) 압축/해제 (포맷 설명은 format.rs)

use crate::block::{self, BlockReader, BlockWriter};
use crate::format::{self, CODEC_ZSTD, EntryHeader, EntryKind, HEADER_SIZE, Header};
use crate::sink::Sink;
use crate::{ArchiveInfo, ArchiveKind, CompressOptions, Control, EntryInfo, HashingReader, Stats};
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub(crate) fn compress(inputs: &[PathBuf], output: &Path, opts: &CompressOptions) -> Result<Stats> {
    let mut file = File::create(output)
        .with_context(|| format!("출력 파일을 만들 수 없음: {}", output.display()))?;
    let mut stats = Stats::default();
    let entries = crate::collect_inputs(inputs, output, &mut stats)?;

    // 항목 수·전체 크기·인덱스 위치는 아직 모르므로 0으로 둔 헤더를 먼저 쓴다
    let mut header = Header {
        version: format::VERSION,
        codec: CODEC_ZSTD,
        level: opts.level as u8,
        entry_count: 0,
        total_size: 0,
        index_offset: 0,
    };
    header.write_to(&mut file)?;

    // 항목 스트림을 BlockWriter에 쓰면, 4MB씩 모여서 여러 스레드가 동시에 압축한다
    let mut w = BlockWriter::new(file, opts.level, opts.threads);
    for e in &entries {
        let (secs, nanos) = format::split_time(e.mtime);
        match e.kind {
            EntryKind::Dir => {
                EntryHeader {
                    kind: EntryKind::Dir,
                    path: e.archive_path.clone(),
                    mtime_secs: secs,
                    mtime_nanos: nanos,
                    size: 0,
                }
                .write_to(&mut w)?;
                stats.dirs += 1;
            }
            EntryKind::File => {
                let src = File::open(&e.path)
                    .with_context(|| format!("파일을 열 수 없음: {}", e.path.display()))?;
                let size = src.metadata()?.len();
                EntryHeader {
                    kind: EntryKind::File,
                    path: e.archive_path.clone(),
                    mtime_secs: secs,
                    mtime_nanos: nanos,
                    size,
                }
                .write_to(&mut w)?;
                // 정확히 size 바이트만 복사 (압축 중에 파일이 커져도 포맷이 깨지지 않게)
                let mut reader = HashingReader::new(
                    BufReader::with_capacity(crate::BUF_SIZE, src).take(size),
                    opts.control.clone(),
                );
                io::copy(&mut reader, &mut w)?;
                if reader.bytes != size {
                    bail!("압축 중에 파일 크기가 바뀌었습니다: {}", e.path.display());
                }
                w.write_all(&reader.hasher.finalize().to_le_bytes())?;
                stats.files += 1;
                stats.total_size += size;
            }
        }
    }
    format::write_end(&mut w)?;

    let (mut file, metas) = w.finish()?;

    // 블록 인덱스를 끝에 쓰고, 헤더로 돌아가서 실제 값을 채운다
    header.index_offset = file.stream_position()?;
    {
        let mut iw = io::BufWriter::new(&mut file);
        block::write_index(&mut iw, &metas)?;
        iw.flush()?;
    }
    header.entry_count = stats.files + stats.dirs;
    header.total_size = stats.total_size;
    file.seek(SeekFrom::Start(0))?;
    header.write_to(&mut file)?;
    file.sync_all()?;
    Ok(stats)
}

pub(crate) fn extract(
    input: &Path,
    sink: &mut Sink,
    control: Option<Control>,
    mut on_entry: impl FnMut(EntryInfo),
) -> Result<ArchiveInfo> {
    let _ = control; // 진행률·취소는 sink 안의 HashingReader가 맡는다
    let mut file = File::open(input)
        .with_context(|| format!("압축 파일을 열 수 없음: {}", input.display()))?;
    let header = Header::read_from(&mut file)?;

    // 항목 스트림을 읽을 Reader: v3은 블록 병렬 해제, v2는 zstd 스트림 하나
    let stream: Box<dyn Read> = if header.version >= 3 {
        file.seek(SeekFrom::Start(header.index_offset))?;
        let metas = block::read_index(&mut BufReader::new(&mut file))?;
        file.seek(SeekFrom::Start(HEADER_SIZE))?;
        Box::new(BlockReader::new(file, metas, crate::num_threads()))
    } else {
        let raw = BufReader::with_capacity(crate::BUF_SIZE, file);
        Box::new(zstd::stream::Decoder::with_buffer(raw)?)
    };
    // 항목 머리말처럼 작은 읽기가 많으므로 한 번 더 버퍼링
    let mut r = BufReader::with_capacity(crate::BUF_SIZE, stream);

    let mut count = 0u64;
    while let Some(entry) = EntryHeader::read_from(&mut r)? {
        on_entry(EntryInfo { header: &entry });
        count += 1;
        match entry.kind {
            EntryKind::Dir => sink.dir(&entry.path, Some(entry.mtime()))?,
            EntryKind::File => {
                let mut data = (&mut r).take(entry.size);
                let (copied, actual) =
                    sink.file(&entry.path, entry.size, Some(entry.mtime()), &mut data)?;
                if copied != entry.size {
                    bail!("압축 파일이 중간에 끊겼습니다: {} (파일 손상)", entry.path);
                }
                let mut crc = [0u8; 4];
                r.read_exact(&mut crc)
                    .map_err(|_| anyhow::anyhow!("압축 파일이 중간에 끊겼습니다 (파일 손상)"))?;
                let expected = u32::from_le_bytes(crc);
                if expected != actual {
                    bail!(
                        "CRC32 불일치: {} (기대 {expected:08x}, 실제 {actual:08x}) — 파일 손상",
                        entry.path
                    );
                }
            }
        }
    }

    if count != header.entry_count {
        bail!(
            "항목 수 불일치: 헤더 {} 개, 실제 {} 개 (파일 손상)",
            header.entry_count,
            count
        );
    }
    Ok(ArchiveInfo {
        kind: ArchiveKind::Pouch,
        entry_count: header.entry_count,
        total_size: header.total_size,
    })
}
