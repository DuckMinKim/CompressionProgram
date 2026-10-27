//! Pouch GUI — 끌어다 놓아서 압축(bzx·zip)하고, 압축 파일(bzx·zip·7z·tar…)을 열어서 푸는 창 프로그램

// 릴리스 빌드에서는 검은 콘솔 창이 뜨지 않게 한다 (Windows)
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, RichText};
use pouch::format::{EntryHeader, EntryKind};
use pouch::{ArchiveInfo, CompressOptions, Conflict, Control, OutFormat, human_size};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const ACCENT: Color32 = Color32::from_rgb(0x2f, 0x6f, 0xeb);
const OK_GREEN: Color32 = Color32::from_rgb(0x1f, 0x9d, 0x55);
const ERR_RED: Color32 = Color32::from_rgb(0xd9, 0x3a, 0x3a);
/// 시작할 때 한 번만 확인 (관리자 권한 여부는 실행 중에 바뀌지 않음)
static ELEVATED: std::sync::LazyLock<bool> = std::sync::LazyLock::new(pouch::assoc::is_elevated);

const WARN: Color32 = Color32::from_rgb(0xd9, 0x8a, 0x1a);

fn main() -> eframe::Result {
    // 실행 인자로 받은 경로 (탐색기에서 .bzx 더블클릭, exe 위에 끌어다 놓기 등)
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Pouch")
            .with_inner_size([720.0, 580.0])
            .with_min_inner_size([520.0, 440.0])
            .with_drag_and_drop(true)
            .with_icon(
                eframe::icon_data::from_png_bytes(pouch::assoc::ICON_PNG).unwrap_or_default(),
            ),
        ..Default::default()
    };

    eframe::run_native(
        "pouch",
        options,
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);
            setup_style(&cc.egui_ctx);
            let mut app = App::default();
            app.add_paths(args);
            Ok(Box::new(app))
        }),
    )
}

// ───────────────────────────── 상태 ─────────────────────────────

#[derive(Default)]
struct App {
    mode: Mode,
    /// 풀 때 같은 이름이 있으면 어떻게 할지 (다른 압축 파일을 열어도 유지)
    conflict: Conflict,
    /// 만들 형식 (다음 압축에도 유지)
    format: OutFormat,
    job: Option<Job>,
    status: Option<Status>,
}

#[derive(Default)]
enum Mode {
    #[default]
    Empty,
    Compress(CompressState),
    Extract(ExtractState),
}

struct CompressState {
    inputs: Vec<PathBuf>,
    output: PathBuf,
    /// 사용자가 저장 위치를 직접 골랐는지 (그렇다면 항목이 바뀌어도 유지)
    output_custom: bool,
    level: i32,
}

struct ExtractState {
    archive: PathBuf,
    listing: Listing,
    out_dir: PathBuf,
    into_subfolder: bool,
}

/// 목록 읽기 결과: (요약, 항목들, 압축 파일 크기)
type ListResult = Result<(ArchiveInfo, Vec<EntryHeader>, u64), String>;

enum Listing {
    Loading(Receiver<ListResult>),
    Ready {
        info: ArchiveInfo,
        entries: Vec<EntryHeader>,
        archive_size: u64,
    },
    Failed(String),
}

/// 백그라운드에서 돌고 있는 압축/해제 작업
struct Job {
    title: String,
    control: Control,
    total: Arc<AtomicU64>,
    started: Instant,
    rx: Receiver<JobResult>,
}

enum JobResult {
    Done { text: String, reveal: PathBuf },
    Failed(String),
    Cancelled(String),
}

struct Status {
    kind: StatusKind,
    text: String,
    reveal: Option<PathBuf>,
}

#[derive(PartialEq)]
enum StatusKind {
    Ok,
    Error,
    Info,
}

/// UI를 그리는 동안 모아 두었다가 나중에 처리할 동작
enum Action {
    AddFiles,
    AddFolder,
    OpenArchive,
    Reset,
    RemoveInput(usize),
    ChooseOutput,
    ChooseOutDir,
    StartCompress,
    StartExtract,
    Cancel,
    Reveal(PathBuf),
    Dropped(Vec<PathBuf>),
    /// 형식(bzx/zip)을 바꿈 → 저장 파일 확장자도 바꾼다
    FormatChanged,
    /// .bzx 파일을 이 프로그램과 연결 (true) / 해제 (false)
    #[cfg_attr(not(windows), allow(dead_code))]
    Associate(bool),
}

// ───────────────────────────── 동작 ─────────────────────────────

impl App {
    fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// 경로들이 들어왔을 때: 압축 파일 하나면 풀기 화면, 아니면 압축 목록에 추가
    fn add_paths(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        if paths.len() == 1 && is_archive(&paths[0]) && paths[0].is_file() {
            self.open_archive(paths[0].clone());
            return;
        }
        let mut inputs = match std::mem::take(&mut self.mode) {
            Mode::Compress(s) => s,
            _ => CompressState {
                inputs: Vec::new(),
                output: PathBuf::new(),
                output_custom: false,
                level: 3,
            },
        };
        for p in paths {
            if !inputs.inputs.contains(&p) {
                inputs.inputs.push(p);
            }
        }
        self.mode = Mode::Compress(inputs);
        self.refresh_default_output();
        self.status = None;
    }

    fn refresh_default_output(&mut self) {
        if let Mode::Compress(s) = &mut self.mode
            && !s.output_custom
            && let Ok(p) = pouch::default_archive_path_for(&s.inputs, self.format)
        {
            s.output = p;
        }
    }

    fn open_archive(&mut self, archive: PathBuf) {
        let (tx, rx) = mpsc::channel();
        let path = archive.clone();
        std::thread::spawn(move || {
            let r = pouch::list_archive(&path)
                .map(|(h, e)| {
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    (h, e, size)
                })
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(r);
        });
        let out_dir = archive
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        self.mode = Mode::Extract(ExtractState {
            archive,
            listing: Listing::Loading(rx),
            out_dir,
            into_subfolder: false,
        });
        self.status = None;
    }

    fn start_compress(&mut self, ctx: &egui::Context) {
        let Mode::Compress(s) = &self.mode else {
            return;
        };
        if s.inputs.is_empty() {
            return;
        }
        let inputs = s.inputs.clone();
        let output = s.output.clone();
        let level = s.level;
        let format = self.format;
        let control = Control::default();
        let total = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel();
        let (c2, t2, ctx2) = (control.clone(), total.clone(), ctx.clone());

        std::thread::spawn(move || {
            t2.store(pouch::scan_total_size(&inputs), Ordering::Relaxed);
            let start = Instant::now();
            let opts = CompressOptions {
                level,
                threads: pouch::num_threads(),
                control: Some(c2),
                format,
            };
            let result = match pouch::compress_paths(&inputs, &output, &opts) {
                Ok(stats) => {
                    let secs = start.elapsed().as_secs_f64();
                    let compressed = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
                    JobResult::Done {
                        text: format!(
                            "압축 완료 — 파일 {}개 · {} → {} ({}) · {:.1}초",
                            stats.files,
                            human_size(stats.total_size),
                            human_size(compressed),
                            percent(compressed, stats.total_size),
                            secs
                        ),
                        reveal: output,
                    }
                }
                Err(e) if pouch::is_cancelled_error(&e) => {
                    JobResult::Cancelled("압축을 취소했습니다.".into())
                }
                Err(e) => JobResult::Failed(format!("{e:#}")),
            };
            let _ = tx.send(result);
            ctx2.request_repaint();
        });

        self.job = Some(Job {
            title: "압축하는 중".into(),
            control,
            total,
            started: Instant::now(),
            rx,
        });
        self.status = None;
    }

    fn start_extract(&mut self, ctx: &egui::Context) {
        let Mode::Extract(s) = &self.mode else { return };
        let Listing::Ready { info, .. } = &s.listing else {
            return;
        };
        let archive = s.archive.clone();
        let conflict = self.conflict;
        let target = extract_target(s, conflict);
        // "폴더 만들어서 풀기"에서 폴더 이름 자체에 번호가 붙었는지
        let folder_renamed =
            (s.into_subfolder && target != s.out_dir.join(archive_stem(&s.archive))).then(|| {
                (
                    archive_stem(&s.archive),
                    target
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                )
            });
        let control = Control::default();
        let total = Arc::new(AtomicU64::new(info.total_size));
        let (tx, rx) = mpsc::channel();
        let (c2, ctx2) = (control.clone(), ctx.clone());

        std::thread::spawn(move || {
            let start = Instant::now();
            let result = match pouch::extract_archive_with(
                &archive,
                Some(&target),
                conflict,
                Some(c2),
                |_| {},
            ) {
                Ok((_, stats)) => {
                    let mut text = format!(
                        "압축 풀기 완료 — 파일 {}개 · {} · {:.1}초 (손상 없음)",
                        stats.files,
                        human_size(stats.total_size),
                        start.elapsed().as_secs_f64()
                    );
                    if let Some((from, to)) = folder_renamed.as_ref().or(stats.renamed.first()) {
                        text += &format!("\n이름이 겹쳐서 '{from}' → '{to}'(으)로 풀었습니다");
                        if stats.renamed.len() > 1 {
                            text += &format!(" 외 {}개", stats.renamed.len() - 1);
                        }
                    }
                    JobResult::Done {
                        text,
                        reveal: target,
                    }
                }
                Err(e) if pouch::is_cancelled_error(&e) => JobResult::Cancelled(
                    "압축 풀기를 취소했습니다. 이미 풀린 파일은 남아 있습니다.".into(),
                ),
                Err(e) => JobResult::Failed(format!("{e:#}")),
            };
            let _ = tx.send(result);
            ctx2.request_repaint();
        });

        self.job = Some(Job {
            title: "압축 푸는 중".into(),
            control,
            total,
            started: Instant::now(),
            rx,
        });
        self.status = None;
    }

    /// 백그라운드 작업·목록 읽기 결과를 받아 온다
    fn poll(&mut self) {
        if let Some(job) = &self.job
            && let Ok(result) = job.rx.try_recv()
        {
            self.status = Some(match result {
                JobResult::Done { text, reveal } => Status {
                    kind: StatusKind::Ok,
                    text,
                    reveal: Some(reveal),
                },
                JobResult::Failed(text) => Status {
                    kind: StatusKind::Error,
                    text,
                    reveal: None,
                },
                JobResult::Cancelled(text) => Status {
                    kind: StatusKind::Info,
                    text,
                    reveal: None,
                },
            });
            self.job = None;
        }
        if let Mode::Extract(s) = &mut self.mode
            && let Listing::Loading(rx) = &s.listing
            && let Ok(r) = rx.try_recv()
        {
            s.listing = match r {
                Ok((info, entries, archive_size)) => Listing::Ready {
                    info,
                    entries,
                    archive_size,
                },
                Err(e) => Listing::Failed(e),
            };
        }
    }

    fn apply(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::AddFiles => {
                if let Some(files) = rfd::FileDialog::new()
                    .set_title("압축할 파일 선택")
                    .pick_files()
                {
                    self.add_paths(files);
                }
            }
            Action::AddFolder => {
                if let Some(dirs) = rfd::FileDialog::new()
                    .set_title("압축할 폴더 선택")
                    .pick_folders()
                {
                    self.add_paths(dirs);
                }
            }
            Action::OpenArchive => {
                if let Some(f) = rfd::FileDialog::new()
                    .set_title("압축 파일 열기")
                    .add_filter(
                        "압축 파일 (bzx, zip, 7z, tar, gz, bz2, xz, zst)",
                        pouch::foreign::SUPPORTED_EXTENSIONS,
                    )
                    .add_filter("모든 파일", &["*"])
                    .pick_file()
                {
                    self.open_archive(f);
                }
            }
            Action::Reset => {
                self.mode = Mode::Empty;
                self.status = None;
            }
            Action::RemoveInput(i) => {
                if let Mode::Compress(s) = &mut self.mode {
                    s.inputs.remove(i);
                    if s.inputs.is_empty() {
                        self.mode = Mode::Empty;
                    }
                }
                self.refresh_default_output();
            }
            Action::ChooseOutput => {
                if let Mode::Compress(s) = &mut self.mode {
                    let ext = self.format.extension();
                    let mut dlg = rfd::FileDialog::new()
                        .set_title("저장 위치")
                        .add_filter(format!("{ext} 압축 파일"), &[ext]);
                    if let Some(dir) = s.output.parent() {
                        dlg = dlg.set_directory(dir);
                    }
                    if let Some(name) = s.output.file_name() {
                        dlg = dlg.set_file_name(name.to_string_lossy());
                    }
                    if let Some(mut p) = dlg.save_file() {
                        if !p.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext)) {
                            p.as_mut_os_string().push(format!(".{ext}"));
                        }
                        s.output = p;
                        s.output_custom = true;
                    }
                }
            }
            Action::ChooseOutDir => {
                if let Mode::Extract(s) = &mut self.mode
                    && let Some(d) = rfd::FileDialog::new()
                        .set_title("풀 위치 선택")
                        .set_directory(&s.out_dir)
                        .pick_folder()
                {
                    s.out_dir = d;
                }
            }
            Action::StartCompress => self.start_compress(ctx),
            Action::StartExtract => self.start_extract(ctx),
            Action::Cancel => {
                if let Some(job) = &self.job {
                    job.control.cancel();
                }
            }
            Action::Reveal(p) => reveal_in_explorer(&p),
            Action::Dropped(paths) => self.add_paths(paths),
            Action::Associate(on) => self.associate(on),
            Action::FormatChanged => {
                let ext = self.format.extension();
                if let Mode::Compress(s) = &mut self.mode
                    && s.output_custom
                {
                    s.output.set_extension(ext);
                }
                self.refresh_default_output();
            }
        }
    }
}

impl App {
    /// .bzx 파일 아이콘·더블클릭 연결 (Windows)
    #[cfg(windows)]
    fn associate(&mut self, on: bool) {
        let result = std::env::current_exe()
            .map_err(anyhow::Error::from)
            .and_then(|exe| {
                if on {
                    pouch::assoc::register(&exe)
                } else {
                    pouch::assoc::unregister()
                }
            });
        self.status = Some(match (result, on) {
            (Ok(()), true) => Status {
                kind: StatusKind::Ok,
                text: "이제 .bzx 파일에 Pouch 아이콘이 보이고, 더블클릭하면 Pouch로 열립니다."
                    .into(),
                reveal: None,
            },
            (Ok(()), false) => Status {
                kind: StatusKind::Info,
                text: ".bzx 파일 연결을 해제했습니다.".into(),
                reveal: None,
            },
            (Err(e), _) => Status {
                kind: StatusKind::Error,
                text: format!("{e:#}"),
                reveal: None,
            },
        });
    }

    #[cfg(not(windows))]
    fn associate(&mut self, _on: bool) {}
}

/// 설정 메뉴 내용
fn settings_menu(ui: &mut egui::Ui, actions: &mut Vec<Action>) {
    #[cfg(windows)]
    {
        let registered = std::env::current_exe()
            .map(|exe| pouch::assoc::is_registered(&exe))
            .unwrap_or(false);
        if registered {
            ui.label(RichText::new("✔ .bzx 파일이 Pouch와 연결되어 있음").color(OK_GREEN));
            if ui.button("연결 해제").clicked() {
                actions.push(Action::Associate(false));
                ui.close();
            }
        } else {
            if ui
                .button(".bzx 파일에 아이콘 붙이고 Pouch로 열기")
                .clicked()
            {
                actions.push(Action::Associate(true));
                ui.close();
            }
            ui.label(
                RichText::new("관리자 권한 필요 없음 · 언제든 해제 가능")
                    .weak()
                    .small(),
            );
        }
    }
    #[cfg(not(windows))]
    {
        let _ = actions;
        ui.label("파일 연결은 Windows에서만 지원합니다.");
    }
}

// ───────────────────────────── 화면 ─────────────────────────────

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll();
        let mut actions = Vec::new();

        // 끌어다 놓기
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if !dropped.is_empty() && !self.busy() {
            actions.push(Action::Dropped(dropped));
        }
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());

        let busy = self.busy();

        // 위쪽 도구 모음
        egui::Panel::top("toolbar")
            .frame(
                egui::Frame::side_top_panel(ui.style())
                    .inner_margin(egui::Margin::symmetric(12, 10)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!busy, |ui| {
                        if ui.button("➕ 파일 추가").clicked() {
                            actions.push(Action::AddFiles);
                        }
                        if ui.button("📁 폴더 추가").clicked() {
                            actions.push(Action::AddFolder);
                        }
                        if ui.button("📂 압축 파일 열기").clicked() {
                            actions.push(Action::OpenArchive);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.menu_button("⚙ 설정", |ui| settings_menu(ui, &mut actions));
                            if !matches!(self.mode, Mode::Empty) && ui.button("처음으로").clicked()
                            {
                                actions.push(Action::Reset);
                            }
                        });
                    });
                });
            });

        // 관리자 권한 경고: 이 상태에선 탐색기에서 끌어다 놓기가 막힌다
        if *ELEVATED {
            egui::Panel::top("elevated_warning")
                .frame(
                    egui::Frame::new()
                        .fill(Color32::from_rgb(0xfd, 0xf0, 0xd5))
                        .inner_margin(egui::Margin::symmetric(12, 8)),
                )
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "⚠ 관리자 권한으로 실행 중이라 윈도우가 끌어다 놓기를 막고 있습니다. \
                                 일반 PowerShell에서 실행하거나 탐색기에서 pouch-gui.exe를 더블클릭해 주세요. \
                                 (위의 추가 버튼은 그대로 쓸 수 있어요)",
                            )
                            .color(Color32::from_rgb(0x7a, 0x4a, 0x00)),
                        )
                        .wrap(),
                    );
                });
        }

        // 아래쪽 진행률 / 결과
        egui::Panel::bottom("status")
            .frame(
                egui::Frame::side_top_panel(ui.style())
                    .inner_margin(egui::Margin::symmetric(12, 10)),
            )
            .show(ui, |ui| self.status_bar(ui, &mut actions));

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::same(16)))
            .show(ui, |ui| {
                // 작업 중이거나 방금 끝났으면 "덮어씁니다" 경고는 의미가 없다
                let fresh = !busy && self.status.is_none();
                ui.add_enabled_ui(!busy, |ui| match &mut self.mode {
                    Mode::Empty => empty_view(ui),
                    Mode::Compress(s) => {
                        compress_view(ui, s, &mut self.format, fresh, &mut actions)
                    }
                    Mode::Extract(s) => extract_view(ui, s, &mut self.conflict, &mut actions),
                });
            });

        if hovering && !busy {
            drop_overlay(&ctx);
        }

        if self.job.is_some()
            || matches!(
                &self.mode,
                Mode::Extract(ExtractState {
                    listing: Listing::Loading(_),
                    ..
                })
            )
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        for a in actions {
            self.apply(a, &ctx);
        }
    }
}

impl App {
    fn status_bar(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        if let Some(job) = &self.job {
            let total = job.total.load(Ordering::Relaxed);
            let done = job.control.done();
            let secs = job.started.elapsed().as_secs_f64();
            let speed = done as f64 / secs.max(0.001);
            let (frac, text) = if total == 0 {
                (0.0, format!("{} … 준비 중", job.title))
            } else {
                let frac = (done as f64 / total as f64).min(1.0) as f32;
                let remain = if speed > 0.0 {
                    (total.saturating_sub(done)) as f64 / speed
                } else {
                    0.0
                };
                (
                    frac,
                    format!(
                        "{} {:.0}% · {}/s · 남은 시간 약 {}",
                        job.title,
                        frac * 100.0,
                        human_size(speed as u64),
                        human_secs(remain)
                    ),
                )
            };
            ui.horizontal(|ui| {
                ui.add(
                    egui::ProgressBar::new(frac)
                        .text(text)
                        .desired_width(ui.available_width() - 80.0)
                        .fill(ACCENT),
                );
                if ui.button("취소").clicked() {
                    actions.push(Action::Cancel);
                }
            });
        } else if let Some(st) = &self.status {
            ui.horizontal(|ui| {
                let (icon, color) = match st.kind {
                    StatusKind::Ok => ("✔", OK_GREEN),
                    StatusKind::Error => ("✖", ERR_RED),
                    StatusKind::Info => ("ℹ", WARN),
                };
                ui.label(RichText::new(icon).color(color).strong());
                if let Some(p) = &st.reveal {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("폴더 열기").clicked() {
                            actions.push(Action::Reveal(p.clone()));
                        }
                        ui.add(egui::Label::new(RichText::new(&st.text).color(color)).wrap());
                    });
                } else {
                    ui.add(egui::Label::new(RichText::new(&st.text).color(color)).wrap());
                }
            });
        } else {
            ui.label(
                RichText::new("파일·폴더를 끌어다 놓으면 압축, 압축 파일을 놓으면 풀기").weak(),
            );
        }
    }
}

fn empty_view(ui: &mut egui::Ui) {
    let rect = ui.available_rect_before_wrap();
    let painter = ui.painter();
    let stroke_color = ui.visuals().weak_text_color();
    painter.rect_stroke(
        rect.shrink(4.0),
        egui::CornerRadius::same(14),
        egui::Stroke::new(1.5, stroke_color),
        egui::StrokeKind::Inside,
    );
    let center = rect.center();
    painter.text(
        center - egui::vec2(0.0, 34.0),
        egui::Align2::CENTER_CENTER,
        "📦",
        egui::FontId::proportional(44.0),
        ui.visuals().text_color(),
    );
    painter.text(
        center + egui::vec2(0.0, 18.0),
        egui::Align2::CENTER_CENTER,
        "여기에 파일이나 폴더를 끌어다 놓으세요",
        egui::FontId::proportional(19.0),
        ui.visuals().strong_text_color(),
    );
    painter.text(
        center + egui::vec2(0.0, 46.0),
        egui::Align2::CENTER_CENTER,
        "압축 파일(bzx, zip, 7z, tar, gz…)을 놓으면 압축을 풉니다",
        egui::FontId::proportional(14.0),
        stroke_color,
    );
}

fn compress_view(
    ui: &mut egui::Ui,
    s: &mut CompressState,
    format: &mut OutFormat,
    fresh: bool,
    actions: &mut Vec<Action>,
) {
    ui.heading(format!("압축하기 · {}개 항목", s.inputs.len()));
    ui.add_space(8.0);

    egui::Frame::group(ui.style()).show(ui, |ui| {
        egui::ScrollArea::vertical()
            .max_height((ui.available_height() - 190.0).max(80.0))
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (i, p) in s.inputs.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let icon = if p.is_dir() { "📁" } else { "📄" };
                        let name = p
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        ui.label(icon);
                        ui.label(RichText::new(name).strong());
                        if let Some(parent) = p.parent() {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(parent.display().to_string()).weak().small(),
                                )
                                .truncate(),
                            );
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .small_button("✕")
                                .on_hover_text("목록에서 빼기")
                                .clicked()
                            {
                                actions.push(Action::RemoveInput(i));
                            }
                        });
                    });
                }
            });
    });

    ui.add_space(12.0);
    egui::Grid::new("compress_opts")
        .num_columns(2)
        .spacing([14.0, 10.0])
        .show(ui, |ui| {
            ui.label("형식");
            ui.horizontal(|ui| {
                let before = *format;
                ui.radio_value(format, OutFormat::Pouch, "bzx — 가장 빠르고 작게")
                    .on_hover_text("멀티코어 zstd. Pouch로만 열 수 있습니다.");
                ui.radio_value(format, OutFormat::Zip, "zip — 어디서나 열림")
                    .on_hover_text(
                        "윈도우 탐색기·반디집 등 어디서나 열립니다. 멀티코어로 압축합니다.",
                    );
                if *format != before {
                    actions.push(Action::FormatChanged);
                }
            });
            ui.end_row();

            ui.label("저장 위치");
            ui.horizontal(|ui| {
                if ui.button("변경…").clicked() {
                    actions.push(Action::ChooseOutput);
                }
                ui.add(egui::Label::new(s.output.display().to_string()).truncate());
            });
            ui.end_row();

            ui.label("압축 수준");
            ui.horizontal(|ui| {
                ui.add(egui::Slider::new(&mut s.level, 1..=19).show_value(false));
                ui.label(level_label(s.level));
            });
            ui.end_row();
        });

    if fresh && s.output.exists() {
        ui.label(RichText::new("⚠ 같은 이름의 파일이 이미 있어서 덮어씁니다").color(WARN));
    }

    ui.add_space(10.0);
    if big_button(ui, "압축 시작").clicked() {
        actions.push(Action::StartCompress);
    }
}

fn extract_view(
    ui: &mut egui::Ui,
    s: &mut ExtractState,
    conflict: &mut Conflict,
    actions: &mut Vec<Action>,
) {
    let name = s
        .archive
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    ui.heading(format!("압축 풀기 · {name}"));
    ui.add_space(4.0);

    let (info, entries, archive_size) = match &s.listing {
        Listing::Loading(_) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("내용을 읽는 중…");
            });
            return;
        }
        Listing::Failed(e) => {
            ui.label(RichText::new(format!("이 파일을 열 수 없습니다: {e}")).color(ERR_RED));
            return;
        }
        Listing::Ready {
            info,
            entries,
            archive_size,
        } => (info, entries, *archive_size),
    };

    let files = entries.iter().filter(|e| e.kind == EntryKind::File).count();
    ui.label(
        RichText::new(format!(
            "{} · 파일 {}개 · 폴더 {}개 · 원본 {} → 압축 {} ({})",
            info.kind.label(),
            files,
            entries.len() - files,
            human_size(info.total_size),
            human_size(archive_size),
            percent(archive_size, info.total_size)
        ))
        .weak(),
    );
    ui.add_space(8.0);

    egui::Frame::group(ui.style()).show(ui, |ui| {
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
        egui::ScrollArea::vertical()
            .max_height((ui.available_height() - 165.0).max(80.0))
            .auto_shrink([false, true])
            .show_rows(ui, row_h, entries.len(), |ui, range| {
                for e in &entries[range] {
                    ui.horizontal(|ui| {
                        let (icon, size) = match e.kind {
                            EntryKind::Dir => ("📁", String::new()),
                            EntryKind::File => ("📄", human_size(e.size)),
                        };
                        ui.add_sized(
                            [86.0, row_h],
                            egui::Label::new(RichText::new(size).weak().monospace()),
                        );
                        ui.label(icon);
                        ui.add(egui::Label::new(&e.path).truncate());
                    });
                }
            });
    });

    ui.add_space(12.0);
    egui::Grid::new("extract_opts")
        .num_columns(2)
        .spacing([14.0, 8.0])
        .show(ui, |ui| {
            ui.label("풀 위치");
            ui.horizontal(|ui| {
                if ui.button("변경…").clicked() {
                    actions.push(Action::ChooseOutDir);
                }
                let shown = extract_target(s, *conflict);
                ui.add(egui::Label::new(shown.display().to_string()).truncate());
            });
            ui.end_row();

            ui.label("");
            ui.checkbox(
                &mut s.into_subfolder,
                format!("'{}' 폴더를 만들어서 풀기", archive_stem(&s.archive)),
            );
            ui.end_row();

            ui.label("같은 이름");
            ui.horizontal(|ui| {
                ui.radio_value(conflict, Conflict::Rename, "번호 붙이기")
                    .on_hover_text("사진 → 사진 (1), 메모.txt → 메모 (1).txt");
                ui.radio_value(conflict, Conflict::Overwrite, "덮어쓰기")
                    .on_hover_text("기존 파일을 새 파일로 바꿉니다. 폴더는 합쳐집니다.");
                ui.radio_value(conflict, Conflict::Stop, "멈추기")
                    .on_hover_text("겹치는 파일이 있으면 아무것도 바꾸지 않고 멈춥니다.");
            });
            ui.end_row();
        });

    ui.add_space(10.0);
    if big_button(ui, "압축 풀기").clicked() {
        actions.push(Action::StartExtract);
    }
}

fn big_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add_sized(
        [ui.available_width(), 42.0],
        egui::Button::new(
            RichText::new(text)
                .size(17.0)
                .strong()
                .color(Color32::WHITE),
        )
        .fill(ACCENT)
        .corner_radius(8),
    )
}

fn drop_overlay(ctx: &egui::Context) {
    let screen = ctx.content_rect();
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::unique("drop"),
    ));
    painter.rect_filled(
        screen,
        0.0,
        Color32::from_rgba_unmultiplied(47, 111, 235, 60),
    );
    painter.rect_stroke(
        screen.shrink(10.0),
        egui::CornerRadius::same(14),
        egui::Stroke::new(3.0, ACCENT),
        egui::StrokeKind::Inside,
    );
    painter.text(
        screen.center(),
        egui::Align2::CENTER_CENTER,
        "놓으세요!",
        egui::FontId::proportional(28.0),
        ACCENT,
    );
}

// ───────────────────────────── 도우미 ─────────────────────────────

fn level_label(level: i32) -> String {
    let desc = match level {
        1..=2 => "가장 빠름",
        3..=5 => "빠름 (추천)",
        6..=12 => "보통",
        _ => "가장 작게 (느림)",
    };
    format!("레벨 {level} · {desc}")
}

fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        "-".into()
    } else {
        format!("{:.1}%", part as f64 / whole as f64 * 100.0)
    }
}

fn human_secs(s: f64) -> String {
    let s = s.round() as u64;
    if s < 60 {
        format!("{s}초")
    } else {
        format!("{}분 {}초", s / 60, s % 60)
    }
}

/// 실제로 풀 위치. "폴더 만들어서 풀기" + 번호 붙이기면 `demo` → `demo (1)`
fn extract_target(s: &ExtractState, conflict: Conflict) -> PathBuf {
    if !s.into_subfolder {
        return s.out_dir.clone();
    }
    let dir = s.out_dir.join(archive_stem(&s.archive));
    if conflict == Conflict::Rename {
        pouch::unique_path(&dir)
    } else {
        dir
    }
}

fn is_archive(p: &Path) -> bool {
    pouch::foreign::has_archive_extension(p)
}

/// "사진.zip" → "사진", "백업.tar.gz" → "백업"
fn archive_stem(p: &Path) -> String {
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".into());
    match stem.len().checked_sub(4) {
        Some(i) if stem.is_char_boundary(i) && stem[i..].eq_ignore_ascii_case(".tar") && i > 0 => {
            stem[..i].to_string()
        }
        _ => stem,
    }
}

/// 탐색기에서 결과를 보여 준다 (파일이면 그 파일을 선택한 채로)
fn reveal_in_explorer(p: &Path) {
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new("explorer");
        if p.is_file() {
            let mut arg = std::ffi::OsString::from("/select,");
            arg.push(p);
            cmd.arg(arg);
        } else {
            cmd.arg(p);
        }
        let _ = cmd.spawn();
    }
    #[cfg(not(windows))]
    {
        let dir = if p.is_file() {
            p.parent().unwrap_or(p)
        } else {
            p
        };
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let _ = std::process::Command::new(opener).arg(dir).spawn();
    }
}

/// egui 기본 글꼴엔 한글이 없어서, 운영체제의 한글 글꼴을 불러온다
fn setup_fonts(ctx: &egui::Context) {
    const CANDIDATES: [&str; 5] = [
        r"C:\Windows\Fonts\malgun.ttf",               // 맑은 고딕 (Windows)
        "/System/Library/Fonts/AppleSDGothicNeo.ttc", // macOS
        "/usr/share/fonts/truetype/nanum/NanumGothic.ttf", // Linux
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", // Linux
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", // Linux
    ];
    let Some(bytes) = CANDIDATES.iter().find_map(|p| std::fs::read(p).ok()) else {
        return; // 못 찾으면 기본 글꼴 (한글이 네모로 보일 수 있음)
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("korean".into(), Arc::new(egui::FontData::from_owned(bytes)));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        // 영문은 기본 글꼴, 한글은 이 글꼴로
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(1, "korean".into());
    }
    ctx.set_fonts(fonts);
}

fn setup_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        use egui::{FontId, TextStyle};
        style
            .text_styles
            .insert(TextStyle::Heading, FontId::proportional(22.0));
        style
            .text_styles
            .insert(TextStyle::Body, FontId::proportional(15.0));
        style
            .text_styles
            .insert(TextStyle::Button, FontId::proportional(15.0));
        style
            .text_styles
            .insert(TextStyle::Small, FontId::proportional(12.0));
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    });
}
