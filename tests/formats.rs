//! 새 기능 테스트: bzx v3 병렬 블록, zip 만들기, 다른 형식(zip/7z/tar…) 풀기

use bzx::format::{CODEC_ZSTD, EntryHeader, EntryKind, Header};
use bzx::{ArchiveKind, CompressOptions, Conflict, OutFormat, compress_path, extract_archive};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

fn opts(threads: u32, format: OutFormat) -> CompressOptions {
    CompressOptions {
        level: 3,
        threads,
        control: None,
        format,
    }
}

fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (x >> 16) as u8
        })
        .collect()
}

/// 텍스트 비슷한(잘 압축되는) 데이터
fn text(len: usize) -> Vec<u8> {
    let words = [
        "압축",
        "bzx",
        "블록",
        "병렬",
        "zip",
        "데이터",
        "테스트",
        "\n",
    ];
    let mut out = Vec::with_capacity(len);
    let mut i = 0u32;
    while out.len() < len {
        i = i.wrapping_mul(2_654_435_761).wrapping_add(1);
        out.extend_from_slice(words[(i >> 29) as usize].as_bytes());
        out.push(b' ');
    }
    out.truncate(len);
    out
}

/// 여러 종류가 섞인 테스트 폴더
fn sample_tree(base: &Path) -> PathBuf {
    let root = base.join("샘플 폴더");
    fs::create_dir_all(root.join("문서/깊은 곳")).unwrap();
    fs::create_dir_all(root.join("빈 폴더")).unwrap();
    fs::write(root.join("빈파일.txt"), "").unwrap();
    fs::write(root.join("메모.txt"), "안녕하세요\n".repeat(300)).unwrap();
    fs::write(root.join("문서/큰 텍스트.txt"), text(9 << 20)).unwrap(); // 여러 블록·여러 조각
    fs::write(root.join("문서/깊은 곳/사진.jpg"), noise(3 << 20, 1)).unwrap(); // 압축 안 되는 데이터
    fs::write(root.join("문서/깊은 곳/data.bin"), noise(700_000, 2)).unwrap();
    for i in 0..50 {
        fs::write(
            root.join(format!("문서/작은{i}.txt")),
            format!("파일 {i}\n").repeat(i + 1),
        )
        .unwrap();
    }
    root
}

fn assert_same_tree(a: &Path, b: &Path) {
    let list = |root: &Path| {
        let mut v: Vec<_> = walkdir::WalkDir::new(root)
            .into_iter()
            .map(|e| {
                let e = e.unwrap();
                let rel = e.path().strip_prefix(root).unwrap().to_path_buf();
                let content = e.file_type().is_file().then(|| fs::read(e.path()).unwrap());
                (rel, content)
            })
            .collect();
        v.sort();
        v
    };
    let (la, lb) = (list(a), list(b));
    assert_eq!(la.len(), lb.len(), "항목 수가 다름");
    for (x, y) in la.iter().zip(lb.iter()) {
        assert_eq!(x.0, y.0);
        assert!(x.1 == y.1, "내용이 다름: {}", x.0.display());
    }
}

// ───────────────────────────── bzx v3 ─────────────────────────────

#[test]
fn bzx_v3_multiblock_parallel_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let root = sample_tree(dir.path());
    let archive = dir.path().join("a.bzx");
    compress_path(&root, &archive, &opts(4, OutFormat::Bzx)).unwrap();

    let (h, _) = bzx::read_header(&archive).unwrap();
    assert_eq!(h.version, 3);
    assert!(h.index_offset > 0);

    let out = dir.path().join("out");
    let (info, stats) = extract_archive(&archive, Some(&out), Conflict::Stop, |_| {}).unwrap();
    assert_eq!(info.kind, ArchiveKind::Bzx);
    assert_eq!(stats.files, 55);
    assert_same_tree(&root, &out.join("샘플 폴더"));
}

#[test]
fn bzx_same_result_with_any_thread_count() {
    // 스레드 수와 상관없이 같은 파일이 나와야 한다 (블록 순서 보장)
    let dir = tempfile::tempdir().unwrap();
    let root = sample_tree(dir.path());
    let a1 = dir.path().join("t1.bzx");
    let a4 = dir.path().join("t4.bzx");
    compress_path(&root, &a1, &opts(1, OutFormat::Bzx)).unwrap();
    compress_path(&root, &a4, &opts(4, OutFormat::Bzx)).unwrap();
    assert_eq!(fs::read(&a1).unwrap(), fs::read(&a4).unwrap());
}

#[test]
fn bzx_incompressible_blocks_are_stored() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("video.mp4");
    let data = noise(12 << 20, 9);
    fs::write(&src, &data).unwrap();
    let archive = dir.path().join("v.bzx");
    compress_path(&src, &archive, &opts(2, OutFormat::Bzx)).unwrap();
    let size = fs::metadata(&archive).unwrap().len();
    // 저장만 했으므로 원본보다 아주 조금만 크다 (헤더·인덱스)
    assert!(size < data.len() as u64 + 4096, "{size}");
    let out = dir.path().join("out");
    extract_archive(&archive, Some(&out), Conflict::Stop, |_| {}).unwrap();
    assert!(fs::read(out.join("video.mp4")).unwrap() == data);
}

#[test]
fn bzx_reads_old_v2_files() {
    // 예전 버전(v2)으로 만든 파일도 열려야 한다
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("old.bzx");
    let mut f = fs::File::create(&archive).unwrap();
    let body = b"version two".to_vec();
    Header {
        version: 2,
        codec: CODEC_ZSTD,
        level: 3,
        entry_count: 2,
        total_size: body.len() as u64,
        index_offset: 0,
    }
    .write_to(&mut f)
    .unwrap();
    let mut enc = zstd::stream::Encoder::new(f, 3).unwrap();
    for (kind, path, size) in [
        (EntryKind::Dir, "옛날", 0),
        (EntryKind::File, "옛날/a.txt", body.len() as u64),
    ] {
        EntryHeader {
            kind,
            path: path.into(),
            mtime_secs: 1_600_000_000,
            mtime_nanos: 0,
            size,
        }
        .write_to(&mut enc)
        .unwrap();
    }
    enc.write_all(&body).unwrap();
    enc.write_all(&crc32fast::hash(&body).to_le_bytes())
        .unwrap();
    bzx::format::write_end(&mut enc).unwrap();
    enc.finish().unwrap();

    let out = dir.path().join("out");
    extract_archive(&archive, Some(&out), Conflict::Stop, |_| {}).unwrap();
    assert_eq!(fs::read(out.join("옛날/a.txt")).unwrap(), body);
}

#[test]
fn many_small_files_parallel_write() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("작은 파일들");
    for d in 0..20 {
        fs::create_dir_all(root.join(format!("d{d}"))).unwrap();
        for f in 0..150 {
            fs::write(
                root.join(format!("d{d}/f{f}.txt")),
                format!("{d}-{f}").repeat(f + 1),
            )
            .unwrap();
        }
    }
    let archive = dir.path().join("s.bzx");
    compress_path(&root, &archive, &opts(4, OutFormat::Bzx)).unwrap();
    let out = dir.path().join("out");
    let (_, stats) = extract_archive(&archive, Some(&out), Conflict::Stop, |_| {}).unwrap();
    assert_eq!(stats.files, 3000);
    assert_same_tree(&root, &out.join("작은 파일들"));
}

#[test]
fn bzx_corrupt_index_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("t.txt");
    fs::write(&src, text(6 << 20)).unwrap();
    let archive = dir.path().join("t.bzx");
    compress_path(&src, &archive, &opts(2, OutFormat::Bzx)).unwrap();
    let mut bytes = fs::read(&archive).unwrap();
    let n = bytes.len();
    bytes[n - 3] ^= 0x55; // 인덱스의 마지막 블록 크기를 망가뜨림
    fs::write(&archive, bytes).unwrap();
    assert!(extract_archive(&archive, None, Conflict::Stop, |_| {}).is_err());
}

// ───────────────────────────── zip 만들기 ─────────────────────────────

#[test]
fn zip_roundtrip_with_parallel_deflate() {
    let dir = tempfile::tempdir().unwrap();
    let root = sample_tree(dir.path());
    let archive = dir.path().join("a.zip");
    compress_path(&root, &archive, &opts(4, OutFormat::Zip)).unwrap();

    // 1) 다른 라이브러리(zip 크레이트)로 직접 열어서 CRC까지 확인
    let mut z = zip::ZipArchive::new(fs::File::open(&archive).unwrap()).unwrap();
    for i in 0..z.len() {
        let mut f = z.by_index(i).unwrap();
        std::io::copy(&mut f, &mut std::io::sink()).expect("CRC 오류 없이 읽혀야 함");
    }
    // 2) 우리 해제기로 풀어서 원본과 비교
    let out = dir.path().join("out");
    let (info, _) = extract_archive(&archive, Some(&out), Conflict::Stop, |_| {}).unwrap();
    assert_eq!(info.kind, ArchiveKind::Zip);
    assert_same_tree(&root, &out.join("샘플 폴더"));
}

#[test]
fn zip_is_smaller_than_input_and_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("t.txt");
    fs::write(&src, text(5 << 20)).unwrap();
    let a1 = dir.path().join("1.zip");
    let a4 = dir.path().join("4.zip");
    compress_path(&src, &a1, &opts(1, OutFormat::Zip)).unwrap();
    compress_path(&src, &a4, &opts(4, OutFormat::Zip)).unwrap();
    assert_eq!(fs::read(&a1).unwrap(), fs::read(&a4).unwrap());
    assert!(fs::metadata(&a1).unwrap().len() < (5 << 20) / 2);
}

#[test]
fn zip_with_cp949_korean_names() {
    // 윈도우 기본 압축 기능처럼 이름을 CP949로 넣은 zip
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("cp949.zip");
    {
        let mut w = zip::ZipWriter::new(fs::File::create(&archive).unwrap());
        w.start_file("AAAA.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(b"hello").unwrap();
        w.finish().unwrap();
    }
    // "AAAA" 자리에 CP949 "한글"(C7 D1 B1 DB)을 넣는다 (로컬 헤더·중앙 디렉터리 둘 다)
    let mut bytes = fs::read(&archive).unwrap();
    let mut i = 0;
    while let Some(p) = bytes[i..].windows(8).position(|w| w == b"AAAA.txt") {
        bytes[i + p..i + p + 4].copy_from_slice(&[0xC7, 0xD1, 0xB1, 0xDB]);
        i += p + 8;
    }
    fs::write(&archive, bytes).unwrap();

    let out = dir.path().join("out");
    extract_archive(&archive, Some(&out), Conflict::Stop, |_| {}).unwrap();
    assert_eq!(fs::read(out.join("한글.txt")).unwrap(), b"hello");
}

// ───────────────────────────── tar 계열 ─────────────────────────────

fn make_tar(root: &Path) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    b.append_dir_all("샘플 폴더", root).unwrap();
    b.into_inner().unwrap()
}

#[test]
fn tar_family_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let root = sample_tree(dir.path());
    let tar_bytes = make_tar(&root);

    let gz = {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(&tar_bytes).unwrap();
        e.finish().unwrap()
    };
    let bz2 = {
        let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
        e.write_all(&tar_bytes).unwrap();
        e.finish().unwrap()
    };
    let xz = {
        let mut e =
            lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(1)).unwrap();
        e.write_all(&tar_bytes).unwrap();
        e.finish().unwrap()
    };
    let zst = zstd::encode_all(&tar_bytes[..], 3).unwrap();

    for (name, bytes, label) in [
        ("a.tar", tar_bytes.clone(), "tar"),
        ("a.tar.gz", gz, "tar.gz"),
        ("a.tar.bz2", bz2, "tar.bz2"),
        ("a.tar.xz", xz, "tar.xz"),
        ("a.tar.zst", zst, "tar.zst"),
    ] {
        let archive = dir.path().join(name);
        fs::write(&archive, bytes).unwrap();
        let out = dir.path().join(format!("out-{name}"));
        let (info, _) = extract_archive(&archive, Some(&out), Conflict::Stop, |_| {}).unwrap();
        assert_eq!(info.kind.label(), label);
        assert_same_tree(&root, &out.join("샘플 폴더"));
    }
}

#[test]
fn tar_path_traversal_is_blocked_and_absolute_paths_are_made_relative() {
    let dir = tempfile::tempdir().unwrap();
    let make = |name: &str| {
        let mut h = tar::Header::new_gnu();
        let raw = &mut h.as_old_mut().name;
        raw[..name.len()].copy_from_slice(name.as_bytes());
        h.set_size(4);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(0o644);
        h.set_cksum();
        let mut b = tar::Builder::new(Vec::new());
        b.append(&h, &b"evil"[..]).unwrap();
        b.into_inner().unwrap()
    };

    let evil = dir.path().join("evil.tar");
    fs::write(&evil, make("../evil.txt")).unwrap();
    let out = dir.path().join("out");
    let err = extract_archive(&evil, Some(&out), Conflict::Stop, |_| {}).unwrap_err();
    assert!(format!("{err:#}").contains("안전하지 않은 경로"), "{err:#}");
    assert!(!dir.path().join("evil.txt").exists());

    let abs = dir.path().join("abs.tar");
    fs::write(&abs, make("/abs/x.txt")).unwrap();
    let out2 = dir.path().join("out2");
    extract_archive(&abs, Some(&out2), Conflict::Stop, |_| {}).unwrap();
    assert_eq!(fs::read(out2.join("abs/x.txt")).unwrap(), b"evil");
}

#[test]
fn single_gz_file() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("로그.txt.gz");
    let mut e = flate2::write::GzEncoder::new(
        fs::File::create(&archive).unwrap(),
        flate2::Compression::default(),
    );
    e.write_all(&text(200_000)).unwrap();
    e.finish().unwrap();
    let out = dir.path().join("out");
    let (info, _) = extract_archive(&archive, Some(&out), Conflict::Stop, |_| {}).unwrap();
    assert_eq!(info.kind.label(), "gz");
    assert_eq!(fs::read(out.join("로그.txt")).unwrap(), text(200_000));
}

// ───────────────────────────── 7z ─────────────────────────────

#[test]
fn sevenz_roundtrip_and_fast_list() {
    let dir = tempfile::tempdir().unwrap();
    let root = sample_tree(dir.path());
    let archive = dir.path().join("a.7z");
    sevenz_rust2::compress_to_path(&root, &archive).unwrap();

    let (info, entries) = bzx::list_archive(&archive).unwrap();
    assert_eq!(info.kind, ArchiveKind::SevenZ);
    assert!(
        entries
            .iter()
            .any(|e| e.path.ends_with("큰 텍스트.txt") && e.size == 9 << 20)
    );

    let out = dir.path().join("out");
    extract_archive(&archive, Some(&out), Conflict::Stop, |_| {}).unwrap();
    assert_same_tree(&root, &out);
}

// ───────────────────────────── 공통 ─────────────────────────────

#[test]
fn detects_format_by_content_not_extension() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.txt");
    fs::write(&src, "content").unwrap();
    let archive = dir.path().join("이름만 이상함.dat");
    compress_path(&src, &archive, &opts(1, OutFormat::Zip)).unwrap();
    assert_eq!(bzx::foreign::detect(&archive).unwrap(), ArchiveKind::Zip);
}

#[test]
fn rename_conflict_works_for_zip_too() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("사진");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.txt"), "a").unwrap();
    let archive = dir.path().join("사진.zip");
    compress_path(&root, &archive, &opts(2, OutFormat::Zip)).unwrap();
    let (_, stats) = extract_archive(&archive, Some(dir.path()), Conflict::Rename, |_| {}).unwrap();
    assert_eq!(
        stats.renamed,
        vec![("사진".to_string(), "사진 (1)".to_string())]
    );
    assert_eq!(fs::read(dir.path().join("사진 (1)/a.txt")).unwrap(), b"a");
}

#[test]
fn foreign_names_with_windows_forbidden_chars_are_renamed() {
    assert_eq!(
        bzx::format::normalize_foreign_path("./회의 10:30/a?b*.txt"),
        "회의 10_30/a_b_.txt"
    );
    assert_eq!(
        bzx::format::normalize_foreign_path("C:\\x\\y.txt"),
        "C_/x/y.txt"
    );
    // ".."는 그대로 남아서 거부된다
    assert!(
        bzx::format::safe_relative_path(&bzx::format::normalize_foreign_path("a/../../b")).is_err()
    );
}
