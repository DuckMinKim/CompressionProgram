use anyhow::{Result, bail};
use bzx::{
    ArchiveKind, CompressOptions, Conflict, OutFormat, Stats, compress_paths,
    default_archive_path_for, extract_archive, human_size as human, list_archive, num_threads,
    read_header,
};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
#[command(
    name = "bzx",
    version,
    about = "빠른 압축/해제 프로그램 — 만들기: bzx, zip / 풀기: bzx, zip, 7z, tar, tar.gz, tar.bz2, tar.xz, tar.zst"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 파일이나 폴더를 압축 (여러 개 가능)
    #[command(alias = "c")]
    Compress {
        /// 압축할 파일 또는 폴더
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// 출력 파일 (기본: <입력 이름>.bzx 또는 .zip, 입력과 같은 위치)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// 형식: bzx (가장 빠르고 작음, 기본) / zip (어디서나 열림). -o 의 확장자가 .zip이면 zip
        #[arg(long, value_enum)]
        format: Option<FormatArg>,
        /// 압축 레벨 1~22 (높을수록 작고 느림)
        #[arg(short, long, default_value_t = 3, value_parser = clap::value_parser!(i32).range(1..=22))]
        level: i32,
        /// 스레드 수 (기본: CPU 코어 수)
        #[arg(short = 't', long)]
        threads: Option<u32>,
        /// 출력 파일이 있으면 덮어쓰기
        #[arg(short, long)]
        force: bool,
    },
    /// 압축 풀기 (bzx, zip, 7z, tar, tar.gz/bz2/xz/zst, gz/bz2/xz/zst — 형식 자동 감지)
    #[command(alias = "x")]
    Extract {
        /// 풀 압축 파일
        input: PathBuf,
        /// 풀 위치 (기본: 아카이브가 있는 폴더 = "여기에 풀기")
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// 같은 이름이 있을 때: rename(번호 붙이기, 기본) / overwrite(덮어쓰기) / stop(멈춤)
        #[arg(long, value_enum, default_value_t = OnConflict::Rename)]
        on_conflict: OnConflict,
        /// --on-conflict overwrite 와 같음
        #[arg(short, long)]
        force: bool,
    },
    /// 압축 파일 안의 목록 보기
    #[command(alias = "l")]
    List { input: PathBuf },
    /// 아카이브를 풀지 않고 손상 여부만 검사
    #[command(alias = "t")]
    Test { input: PathBuf },
    /// 요약 정보 보기
    Info { input: PathBuf },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum FormatArg {
    Bzx,
    Zip,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum OnConflict {
    Rename,
    Overwrite,
    Stop,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Compress {
            inputs,
            output,
            format,
            level,
            threads,
            force,
        } => {
            let format = match (format, &output) {
                (Some(FormatArg::Zip), _) => OutFormat::Zip,
                (Some(FormatArg::Bzx), _) => OutFormat::Bzx,
                (None, Some(o)) if o.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")) => {
                    OutFormat::Zip
                }
                _ => OutFormat::Bzx,
            };
            let output = match output {
                Some(o) => o,
                None => default_archive_path_for(&inputs, format)?,
            };
            if output.exists() && !force {
                bail!(
                    "{} 이(가) 이미 있습니다. 덮어쓰려면 -f 를 붙이세요.",
                    output.display()
                );
            }
            let opts = CompressOptions {
                level,
                threads: threads.unwrap_or_else(num_threads),
                control: None,
                format,
            };

            let start = Instant::now();
            let stats = compress_paths(&inputs, &output, &opts)?;
            let secs = start.elapsed().as_secs_f64();
            let compressed = std::fs::metadata(&output)?.len();

            println!("압축 완료: {}", output.display());
            print_stats(&stats, compressed, secs);
            for p in &stats.skipped {
                eprintln!("  건너뜀 (링크 등): {}", p.display());
            }
        }
        Command::Extract {
            input,
            output,
            on_conflict,
            force,
        } => {
            let conflict = match (force, on_conflict) {
                (true, _) | (_, OnConflict::Overwrite) => Conflict::Overwrite,
                (_, OnConflict::Rename) => Conflict::Rename,
                (_, OnConflict::Stop) => Conflict::Stop,
            };
            let out_dir = match output {
                Some(o) => o,
                None => match input.parent() {
                    Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
                    _ => PathBuf::from("."),
                },
            };

            let start = Instant::now();
            let (info, stats) = extract_archive(&input, Some(&out_dir), conflict, |_| {})?;
            let secs = start.elapsed().as_secs_f64();
            let compressed = std::fs::metadata(&input)?.len();

            println!(
                "해제 완료 ({}): {} → {}",
                info.kind.label(),
                input.display(),
                out_dir.display()
            );
            for (from, to) in &stats.renamed {
                println!("  이름이 겹쳐서 '{from}' → '{to}'(으)로 풀었습니다");
            }
            print_stats(&stats, compressed, secs);
        }
        Command::List { input } => {
            let (info, entries) = list_archive(&input)?;
            let mut files = 0;
            for h in &entries {
                match h.kind {
                    bzx::format::EntryKind::Dir => println!("{:>12}  {}/", "<폴더>", h.path),
                    bzx::format::EntryKind::File => {
                        files += 1;
                        println!("{:>12}  {}", human(h.size), h.path)
                    }
                }
            }
            println!("─────────────");
            println!(
                "{} · 파일 {}개, 폴더 {}개, 전체 {}",
                info.kind.label(),
                files,
                entries.len() - files,
                human(info.total_size)
            );
        }
        Command::Test { input } => {
            let start = Instant::now();
            let (_, stats) = extract_archive(&input, None, Conflict::Stop, |_| {})?;
            println!(
                "이상 없음: 파일 {}개, 폴더 {}개, {} ({:.3}초)",
                stats.files,
                stats.dirs,
                human(stats.total_size),
                start.elapsed().as_secs_f64()
            );
        }
        Command::Info { input } => {
            let kind = bzx::foreign::detect(&input)?;
            if kind == ArchiveKind::Bzx {
                let (h, payload) = read_header(&input)?;
                println!("형식       : bzx v{} (zstd 레벨 {})", h.version, h.level);
                println!("항목 수    : {}", h.entry_count);
                println!("원본 크기  : {}", human(h.total_size));
                println!("압축 데이터: {}", human(payload));
            } else {
                let (info, _) = list_archive(&input)?;
                println!("형식       : {}", info.kind.label());
                println!("항목 수    : {}", info.entry_count);
                println!("원본 크기  : {}", human(info.total_size));
                println!("파일 크기  : {}", human(std::fs::metadata(&input)?.len()));
            }
        }
    }
    Ok(())
}

fn print_stats(stats: &Stats, compressed: u64, secs: f64) {
    let original = stats.total_size;
    let ratio = if original == 0 {
        0.0
    } else {
        compressed as f64 / original as f64 * 100.0
    };
    let speed = original as f64 / secs.max(1e-9) / (1024.0 * 1024.0);
    println!("  파일 {}개, 폴더 {}개", stats.files, stats.dirs);
    println!(
        "  원본 {} / 압축 {} ({ratio:.1}%)",
        human(original),
        human(compressed)
    );
    println!("  {secs:.3}초, {speed:.1} MB/s");
}
