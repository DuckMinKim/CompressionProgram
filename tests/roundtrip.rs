use bzx::format::{CODEC_ZSTD, EntryHeader, EntryKind, Header};
use bzx::{CompressOptions, Conflict, compress_path, extract_archive};
use std::fs;
use std::io::Write;
use std::path::Path;

fn opts(threads: u32) -> CompressOptions {
    CompressOptions {
        level: 3,
        threads,
        control: None,
        format: bzx::OutFormat::Bzx,
    }
}

/// 두 폴더의 내용(구조 + 파일 내용)이 같은지 비교
fn assert_same_tree(a: &Path, b: &Path) {
    let list = |root: &Path| {
        let mut v: Vec<_> = walkdir::WalkDir::new(root)
            .into_iter()
            .map(|e| {
                let e = e.unwrap();
                let rel = e.path().strip_prefix(root).unwrap().to_path_buf();
                let content = if e.file_type().is_file() {
                    Some(fs::read(e.path()).unwrap())
                } else {
                    None
                };
                (rel, content)
            })
            .collect();
        v.sort();
        v
    };
    assert_eq!(list(a), list(b));
}

#[test]
fn single_file_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("hello.txt");
    fs::write(&src, "안녕하세요 bzx ".repeat(10_000)).unwrap();
    let archive = dir.path().join("hello.txt.bzx");
    let out = dir.path().join("out");

    let stats = compress_path(&src, &archive, &opts(1)).unwrap();
    assert_eq!(stats.files, 1);
    extract_archive(&archive, Some(&out), Conflict::Stop, |_| {}).unwrap();
    assert_eq!(
        fs::read(out.join("hello.txt")).unwrap(),
        fs::read(&src).unwrap()
    );
}

#[test]
fn folder_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("프로젝트");
    fs::create_dir_all(root.join("src/깊은/폴더")).unwrap();
    fs::create_dir_all(root.join("빈폴더")).unwrap();
    fs::write(root.join("README.md"), "# 제목\n".repeat(500)).unwrap();
    fs::write(root.join("빈파일.txt"), "").unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n".repeat(1000)).unwrap();
    // 압축이 잘 안 되는 의사 난수 데이터 5MB
    let mut x: u32 = 7;
    let noise: Vec<u8> = (0..5 << 20)
        .map(|_| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (x >> 16) as u8
        })
        .collect();
    fs::write(root.join("src/깊은/폴더/noise.bin"), &noise).unwrap();

    let archive = dir.path().join("프로젝트.bzx");
    let stats = compress_path(&root, &archive, &opts(4)).unwrap();
    assert_eq!(stats.files, 4);
    assert_eq!(stats.dirs, 5); // 프로젝트, src, 깊은, 폴더, 빈폴더

    let out = dir.path().join("풀린곳");
    let (_, xs) = extract_archive(&archive, Some(&out), Conflict::Stop, |_| {}).unwrap();
    assert_eq!(xs.files, 4);
    assert_same_tree(&root, &out.join("프로젝트"));
}

#[test]
fn refuses_overwrite_without_force() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.txt");
    fs::write(&src, "data").unwrap();
    let archive = dir.path().join("a.txt.bzx");
    compress_path(&src, &archive, &opts(1)).unwrap();

    // 같은 폴더에 풀면 a.txt가 이미 있으므로 실패해야 함
    assert!(extract_archive(&archive, Some(dir.path()), Conflict::Stop, |_| {}).is_err());
    // -f 면 성공
    extract_archive(&archive, Some(dir.path()), Conflict::Overwrite, |_| {}).unwrap();
}

#[test]
fn archive_inside_input_folder_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("folder");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.txt"), "aaa").unwrap();
    let archive = root.join("folder.bzx"); // 압축 대상 폴더 안에 출력
    let stats = compress_path(&root, &archive, &opts(1)).unwrap();
    assert_eq!(stats.files, 1);
}

#[test]
fn detects_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.txt");
    fs::write(&src, "hello world ".repeat(100_000)).unwrap();
    let archive = dir.path().join("a.txt.bzx");
    compress_path(&src, &archive, &opts(1)).unwrap();

    // 압축 데이터 중간의 바이트 하나를 망가뜨림
    let mut bytes = fs::read(&archive).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    fs::write(&archive, bytes).unwrap();

    assert!(extract_archive(&archive, None, Conflict::Stop, |_| {}).is_err());
}

#[test]
fn blocks_path_traversal() {
    // "../evil.txt" 를 담은 악성 아카이브를 직접 만든다 (v2 형식: zstd 스트림 하나)
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("evil.bzx");
    let mut f = fs::File::create(&archive).unwrap();
    Header {
        version: 2,
        index_offset: 0,
        codec: CODEC_ZSTD,
        level: 3,
        entry_count: 1,
        total_size: 4,
    }
    .write_to(&mut f)
    .unwrap();
    let mut enc = zstd::stream::Encoder::new(f, 3).unwrap();
    EntryHeader {
        kind: EntryKind::File,
        path: "../evil.txt".into(),
        mtime_secs: 0,
        mtime_nanos: 0,
        size: 4,
    }
    .write_to(&mut enc)
    .unwrap();
    enc.write_all(b"evil").unwrap();
    enc.write_all(&crc32fast::hash(b"evil").to_le_bytes())
        .unwrap();
    bzx::format::write_end(&mut enc).unwrap();
    enc.finish().unwrap();

    let out = dir.path().join("out");
    let err = extract_archive(&archive, Some(&out), Conflict::Stop, |_| {}).unwrap_err();
    assert!(err.to_string().contains("안전하지 않은 경로"));
    assert!(!dir.path().join("evil.txt").exists());
}

#[test]
fn multiple_inputs_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("폴더B");
    fs::write(&a, "aaa").unwrap();
    fs::create_dir(&b).unwrap();
    fs::write(b.join("b.txt"), "bbb").unwrap();

    let archive = dir.path().join("묶음.bzx");
    let stats = bzx::compress_paths(&[a.clone(), b.clone()], &archive, &opts(2)).unwrap();
    assert_eq!((stats.files, stats.dirs), (2, 1));

    let out = dir.path().join("out");
    extract_archive(&archive, Some(&out), Conflict::Stop, |_| {}).unwrap();
    assert_eq!(fs::read_to_string(out.join("a.txt")).unwrap(), "aaa");
    assert_eq!(fs::read_to_string(out.join("폴더B/b.txt")).unwrap(), "bbb");
}

#[test]
fn cancel_removes_partial_archive() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big.bin");
    fs::write(&src, vec![7u8; 8 << 20]).unwrap();
    let archive = dir.path().join("big.bin.bzx");

    let control = bzx::Control::default();
    control.cancel(); // 시작하자마자 취소된 상태
    let o = CompressOptions {
        level: 3,
        threads: 1,
        control: Some(control),
        format: bzx::OutFormat::Bzx,
    };
    let err = compress_path(&src, &archive, &o).unwrap_err();
    assert!(bzx::is_cancelled_error(&err), "{err:#}");
    assert!(!archive.exists());
}

#[test]
fn progress_counts_all_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("p.bin");
    fs::write(&src, vec![1u8; 3 << 20]).unwrap();
    let control = bzx::Control::default();
    let o = CompressOptions {
        level: 1,
        threads: 1,
        control: Some(control.clone()),
        format: bzx::OutFormat::Bzx,
    };
    compress_path(&src, &dir.path().join("p.bzx"), &o).unwrap();
    assert_eq!(control.done(), 3 << 20);
    assert_eq!(bzx::scan_total_size(&[src]), 3 << 20);
}

#[test]
fn rename_on_conflict_adds_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("사진");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("a.jpg"), "new").unwrap();
    let file = dir.path().join("메모.txt");
    fs::write(&file, "memo").unwrap();
    let archive = dir.path().join("묶음.bzx");
    bzx::compress_paths(&[folder.clone(), file.clone()], &archive, &opts(1)).unwrap();

    // 같은 곳에 두 번 풀기 → 사진 (1), 메모 (1).txt / 사진 (2), 메모 (2).txt
    for n in 1..=2 {
        let (_, stats) =
            extract_archive(&archive, Some(dir.path()), Conflict::Rename, |_| {}).unwrap();
        assert_eq!(stats.renamed.len(), 2);
        assert_eq!(
            fs::read_to_string(dir.path().join(format!("사진 ({n})/a.jpg"))).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(format!("메모 ({n}).txt"))).unwrap(),
            "memo"
        );
    }
    // 원본은 그대로
    assert_eq!(fs::read_to_string(&file).unwrap(), "memo");
}

#[test]
fn rename_does_nothing_without_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.txt");
    fs::write(&src, "x").unwrap();
    let archive = dir.path().join("a.bzx");
    compress_path(&src, &archive, &opts(1)).unwrap();
    let out = dir.path().join("out");
    let (_, stats) = extract_archive(&archive, Some(&out), Conflict::Rename, |_| {}).unwrap();
    assert!(stats.renamed.is_empty());
    assert!(out.join("a.txt").exists());
}

#[test]
fn unique_path_numbering() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("보고서.pdf");
    assert_eq!(bzx::unique_path(&p), p);
    fs::write(&p, "").unwrap();
    assert_eq!(bzx::unique_path(&p), dir.path().join("보고서 (1).pdf"));
    let d = dir.path().join("폴더.v2");
    fs::create_dir(&d).unwrap();
    assert_eq!(bzx::unique_path(&d), dir.path().join("폴더.v2 (1)"));
}
