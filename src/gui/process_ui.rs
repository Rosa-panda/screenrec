//! 「视频处理」页：插帧 / 压缩转码 / 无损剪切 / 转 GIF / 提取音频。

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{self, RichText};
use serde::{Deserialize, Serialize};

use screenrec::encoder::{Backend, Codec};
use screenrec::process::{self, AudioFmt, FpsTarget, InterpOpts, Job, JobSpec, MediaInfo, Op};

use super::{big_button, card, codec_name, fmt_time, App, ACCENT, MUTED, OK_GREEN, WARN};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Interp,
    Compress,
    Cut,
    Gif,
    Audio,
}

/// 视频处理页的选项（随设置一起保存）。
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProcSettings {
    pub kind: Kind,
    /// 插帧：true 按倍数，false 按目标帧率
    pub by_times: bool,
    pub times: u32,
    pub fps: f64,
    pub model: String,
    /// 显卡编号，-1 为自动
    pub gpu: i32,
    pub codec: Codec,
    pub quality: u32,
    /// 压缩：目标高度，0 为保持原始
    pub height: u32,
    /// 压缩：目标帧率，0 为保持原始
    pub out_fps: u32,
    pub gif_width: u32,
    pub gif_fps: u32,
    pub audio: AudioFmt,
    pub use_range: bool,
    pub start: String,
    pub end: String,
}

impl Default for ProcSettings {
    fn default() -> Self {
        Self {
            kind: Kind::Interp,
            by_times: false,
            times: 2,
            fps: 60.0,
            model: "rife-v4.6".into(),
            gpu: 0,
            codec: Codec::H264,
            quality: 18,
            height: 0,
            out_fps: 0,
            gif_width: 640,
            gif_fps: 15,
            audio: AudioFmt::Mp3,
            use_range: false,
            start: String::new(),
            end: String::new(),
        }
    }
}

#[derive(Default)]
pub struct ProcUi {
    pub src: Option<PathBuf>,
    info: Option<Result<MediaInfo, String>>,
    job: Option<Arc<Job>>,
    last_out: Option<PathBuf>,
    error: Option<String>,
    notice: Option<String>,
}

impl ProcUi {
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }
}

fn fps_text(f: f64) -> String {
    if (f - f.round()).abs() < 0.005 {
        format!("{}", f.round() as u64)
    } else {
        format!("{f:.2}")
    }
}

impl App {
    /// 选定源视频并读取它的信息。
    pub(crate) fn set_proc_src(&mut self, path: PathBuf) {
        self.proc.info = Some(match &self.ffmpeg {
            Ok(ffmpeg) => process::probe(ffmpeg, &path).map_err(|e| e.to_string()),
            Err(e) => Err(e.clone()),
        });
        self.proc.src = Some(path);
        self.proc.error = None;
        self.proc.notice = None;
        self.proc.last_out = None;
    }

    fn interp_target(&self) -> FpsTarget {
        let p = &self.s.proc;
        if p.by_times {
            FpsTarget::Times(p.times.max(2))
        } else {
            FpsTarget::Fps(p.fps.max(1.0))
        }
    }

    fn poll_job(&mut self) {
        let Some(job) = &self.proc.job else { return };
        let Some(result) = job.result() else { return };
        match result {
            Ok(()) => {
                let size = std::fs::metadata(&job.dst).map(|m| m.len()).unwrap_or(0);
                self.proc.notice = Some(format!(
                    "已完成  {}  （用时 {}，{:.1} MB）",
                    job.dst.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    fmt_time(job.started.elapsed().as_secs_f64()),
                    size as f64 / 1048576.0
                ));
                self.proc.last_out = Some(job.dst.clone());
            }
            Err(e) => self.proc.error = Some(e),
        }
        self.proc.job = None;
    }

    fn start_job(&mut self) {
        self.proc.error = None;
        self.proc.notice = None;
        let Some(src) = self.proc.src.clone() else { return };
        let ffmpeg = match &self.ffmpeg {
            Ok(p) => p.clone(),
            Err(e) => {
                self.proc.error = Some(e.clone());
                return;
            }
        };
        let p = self.s.proc.clone();
        let (start, end) = if p.use_range || p.kind == Kind::Cut {
            let start = if p.start.trim().is_empty() { Some(0.0) } else { process::parse_time(&p.start) };
            let end = if p.end.trim().is_empty() { Some(None) } else { process::parse_time(&p.end).map(Some) };
            match (start, end) {
                (Some(s), Some(e)) => (s, e),
                _ => {
                    self.proc.error = Some("片段时间格式不对（例：90、1:30、1:02:03）".into());
                    return;
                }
            }
        } else {
            (0.0, None)
        };
        if p.kind == Kind::Cut && start == 0.0 && end.is_none() {
            self.proc.error = Some("请在「片段范围」里填写要保留片段的起点或终点".into());
            return;
        }
        let op = match p.kind {
            Kind::Interp => Op::Interp(InterpOpts {
                target: self.interp_target(),
                model: p.model.clone(),
                gpu: u32::try_from(p.gpu).ok(),
                codec: p.codec,
                backend: Backend::Auto,
                quality: p.quality,
            }),
            Kind::Compress => Op::Compress {
                codec: p.codec,
                backend: Backend::Auto,
                quality: p.quality,
                height: (p.height > 0).then_some(p.height),
                fps: (p.out_fps > 0).then_some(p.out_fps),
            },
            Kind::Cut => Op::Cut,
            Kind::Gif => Op::Gif { width: p.gif_width, fps: p.gif_fps },
            Kind::Audio => Op::Audio(p.audio),
        };
        let rife = if p.kind == Kind::Interp {
            match &self.rife {
                Ok(r) => Some(r.clone()),
                Err(e) => {
                    self.proc.error = Some(e.clone());
                    return;
                }
            }
        } else {
            None
        };
        let info = self.proc.info.as_ref().and_then(|r| r.as_ref().ok());
        let dst = process::default_dst(&src, &op, info);
        self.proc.job = Some(process::start(JobSpec { src, dst, start, end, op, ffmpeg, rife }));
    }

    pub(crate) fn ui_process(&mut self, ui: &mut egui::Ui) {
        self.poll_job();
        let busy = self.proc.busy();
        ui.add_enabled_ui(!busy, |ui| {
            self.proc_source(ui);
            ui.add_space(8.0);
            self.proc_options(ui);
            ui.add_space(8.0);
            self.proc_range(ui);
        });
        ui.add_space(10.0);
        self.proc_run(ui);
    }

    fn proc_source(&mut self, ui: &mut egui::Ui) {
        let mut pick: Option<PathBuf> = None;
        card(ui, "源视频", |ui| {
            match &self.proc.src {
                Some(p) => ui.label(p.display().to_string()),
                None => ui.label(RichText::new("选择一个视频文件，或直接把文件拖进窗口").color(MUTED)),
            };
            match &self.proc.info {
                Some(Ok(i)) => {
                    let audio = i.audio_codec.clone().unwrap_or_else(|| "无音轨".into());
                    ui.label(
                        RichText::new(format!(
                            "{}×{} · {} fps · {} · {} / {}",
                            i.width,
                            i.height,
                            fps_text(i.fps_f64()),
                            fmt_time(i.duration),
                            i.video_codec,
                            audio
                        ))
                        .color(MUTED)
                        .size(12.0),
                    );
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(e.lines().next().unwrap_or("")).color(WARN).size(12.0));
                }
                None => {}
            }
            ui.horizontal(|ui| {
                if ui.button("选择文件…").clicked() {
                    pick = rfd::FileDialog::new()
                        .add_filter("视频", &["mp4", "mkv", "mov", "m4v", "avi", "webm", "flv", "ts", "wmv"])
                        .set_directory(&self.s.output_dir)
                        .pick_file();
                }
                if let Some(f) = &self.last_file {
                    if ui.button("用最近一次录制").clicked() {
                        pick = Some(f.clone());
                    }
                }
            });
        });
        if let Some(p) = pick {
            self.set_proc_src(p);
        }
    }

    fn proc_options(&mut self, ui: &mut egui::Ui) {
        let target = self.interp_target();
        let models = self.rife.as_ref().map(|r| process::rife_models(r)).unwrap_or_default();
        let src_fps = match &self.proc.info {
            Some(Ok(i)) => Some(i.fps),
            _ => None,
        };
        let rife_err = self.rife.as_ref().err().cloned();
        let p = &mut self.s.proc;
        card(ui, "处理方式", |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut p.kind, Kind::Interp, "  插帧  ");
                ui.selectable_value(&mut p.kind, Kind::Compress, "  压缩转码  ");
                ui.selectable_value(&mut p.kind, Kind::Cut, "  无损剪切  ");
                ui.selectable_value(&mut p.kind, Kind::Gif, "  转 GIF  ");
                ui.selectable_value(&mut p.kind, Kind::Audio, "  提取音频  ");
            });
            let hint = |ui: &mut egui::Ui, text: &str| {
                ui.label(RichText::new(text).color(MUTED).size(12.0));
            };
            let codec_row = |ui: &mut egui::Ui, p: &mut ProcSettings| {
                ui.horizontal(|ui| {
                    ui.label("编码");
                    egui::ComboBox::from_id_salt("proc_codec")
                        .width(90.0)
                        .selected_text(codec_name(p.codec))
                        .show_ui(ui, |ui| {
                            for c in [Codec::H264, Codec::Hevc, Codec::Av1] {
                                ui.selectable_value(&mut p.codec, c, codec_name(c));
                            }
                        });
                    ui.add_space(8.0);
                    ui.label("画质 CQ");
                    ui.add(egui::Slider::new(&mut p.quality, 10..=32));
                });
            };
            match p.kind {
                Kind::Interp => {
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut p.by_times, false, "目标帧率");
                        ui.add_enabled(
                            !p.by_times,
                            egui::DragValue::new(&mut p.fps).range(1.0..=1000.0).speed(1.0).max_decimals(3),
                        );
                        for f in [60.0, 90.0, 120.0, 144.0, 240.0] {
                            if ui.small_button(format!("{f}")).clicked() {
                                p.fps = f;
                                p.by_times = false;
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut p.by_times, true, "按倍数    ");
                        for k in [2, 3, 4, 8] {
                            if ui.selectable_label(p.by_times && p.times == k, format!("  {k}×  ")).clicked() {
                                p.times = k;
                                p.by_times = true;
                            }
                        }
                    });
                    if let Some(src) = src_fps {
                        let (n, d) = process::target_fps(src, target);
                        let (s, t) = (src.0 as f64 / src.1 as f64, n as f64 / d as f64);
                        if t > s + 1e-6 {
                            let text = format!("{} fps → {} fps（{:.2} 倍）", fps_text(s), fps_text(t), t / s);
                            ui.label(RichText::new(text).color(OK_GREEN));
                        } else {
                            let text = format!("目标帧率必须高于源视频的 {} fps", fps_text(s));
                            ui.label(RichText::new(text).color(WARN));
                        }
                    }
                    ui.horizontal(|ui| {
                        ui.label("模型");
                        egui::ComboBox::from_id_salt("rife_model").width(110.0).selected_text(p.model.clone()).show_ui(
                            ui,
                            |ui| {
                                for m in &models {
                                    ui.selectable_value(&mut p.model, m.clone(), m);
                                }
                            },
                        );
                        ui.add_space(8.0);
                        ui.label("显卡");
                        let gpu_name = |g: i32| if g < 0 { "自动".to_string() } else { format!("GPU {g}") };
                        egui::ComboBox::from_id_salt("rife_gpu").width(80.0).selected_text(gpu_name(p.gpu)).show_ui(
                            ui,
                            |ui| {
                                for g in [-1, 0, 1, 2, 3] {
                                    ui.selectable_value(&mut p.gpu, g, gpu_name(g));
                                }
                            },
                        );
                    });
                    codec_row(ui, p);
                    hint(ui, "用 RIFE 在显卡上计算中间帧，声音原样保留。耗时与分辨率、时长成正比；处理中会在系统临时目录占用几 GB 空间，结束后自动清理。双显卡笔记本上 GPU 0 通常是独显。");
                    if let Some(e) = &rife_err {
                        ui.label(RichText::new(e).color(WARN).size(12.0));
                    }
                }
                Kind::Compress => {
                    codec_row(ui, p);
                    ui.horizontal(|ui| {
                        ui.label("分辨率");
                        let h_name = |h: u32| if h == 0 { "保持原始".to_string() } else { format!("{h}p") };
                        egui::ComboBox::from_id_salt("proc_height").width(90.0).selected_text(h_name(p.height)).show_ui(
                            ui,
                            |ui| {
                                for h in [0, 2160, 1440, 1080, 720, 480] {
                                    ui.selectable_value(&mut p.height, h, h_name(h));
                                }
                            },
                        );
                        ui.add_space(8.0);
                        ui.label("帧率");
                        let f_name = |f: u32| if f == 0 { "保持原始".to_string() } else { format!("{f} fps") };
                        egui::ComboBox::from_id_salt("proc_fps").width(90.0).selected_text(f_name(p.out_fps)).show_ui(
                            ui,
                            |ui| {
                                for f in [0, 60, 30, 24] {
                                    ui.selectable_value(&mut p.out_fps, f, f_name(f));
                                }
                            },
                        );
                    });
                    hint(ui, "重新编码以缩小体积：CQ 越大文件越小；只会缩小分辨率、降低帧率，不会放大。");
                }
                Kind::Cut => {
                    hint(ui, "不重新编码，速度极快、画质无损；起点会对齐到最近的关键帧。需要精确到帧时，请用「压缩转码」并设置片段范围。");
                }
                Kind::Gif => {
                    ui.horizontal(|ui| {
                        ui.label("宽度");
                        ui.add(egui::Slider::new(&mut p.gif_width, 160..=1280).suffix(" px"));
                        ui.add_space(8.0);
                        ui.label("帧率");
                        ui.add(egui::Slider::new(&mut p.gif_fps, 5..=30));
                    });
                    hint(ui, "GIF 体积很大，建议配合片段范围只截取几秒。");
                }
                Kind::Audio => {
                    ui.horizontal(|ui| {
                        ui.label("格式");
                        ui.selectable_value(&mut p.audio, AudioFmt::Mp3, "  MP3  ");
                        ui.selectable_value(&mut p.audio, AudioFmt::M4a, "  M4A  ");
                        ui.selectable_value(&mut p.audio, AudioFmt::Wav, "  WAV  ");
                    });
                    hint(ui, "导出视频里的第一条音轨。");
                }
            }
        });
    }

    fn proc_range(&mut self, ui: &mut egui::Ui) {
        let p = &mut self.s.proc;
        card(ui, "片段范围", |ui| {
            let forced = p.kind == Kind::Cut;
            if !forced {
                ui.checkbox(&mut p.use_range, "只处理其中一段");
            }
            ui.add_enabled_ui(forced || p.use_range, |ui| {
                ui.horizontal(|ui| {
                    ui.label("从");
                    ui.add(egui::TextEdit::singleline(&mut p.start).desired_width(70.0).hint_text("开头"));
                    ui.label("到");
                    ui.add(egui::TextEdit::singleline(&mut p.end).desired_width(70.0).hint_text("结尾"));
                    ui.label(RichText::new("格式 分:秒，如 1:30").color(MUTED).size(12.0));
                });
            });
        });
    }

    fn proc_run(&mut self, ui: &mut egui::Ui) {
        match self.proc.job.clone() {
            None => {
                let ready = self.proc.src.is_some() && self.ffmpeg.is_ok();
                if big_button(ui, "▶  开始处理", ACCENT, ready) {
                    self.start_job();
                }
            }
            Some(job) => {
                let p = job.progress();
                ui.add(egui::ProgressBar::new(p).show_percentage());
                let used = job.started.elapsed().as_secs_f64();
                let eta = if p > 0.01 {
                    format!(" · 剩余约 {}", fmt_time(used * (1.0 - p as f64) / p as f64))
                } else {
                    String::new()
                };
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{} · 已用 {}{eta}", job.stage(), fmt_time(used))).color(MUTED));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("取消").clicked() {
                            job.cancel();
                        }
                    });
                });
            }
        }
        if let Some(e) = &self.proc.error {
            let first = e.lines().next().unwrap_or("").to_string();
            ui.label(RichText::new(format!("✖ {first}")).color(ACCENT));
            if e.lines().count() > 1 {
                ui.collapsing("详情", |ui| {
                    ui.label(RichText::new(e.clone()).size(12.0).color(MUTED));
                });
            }
        }
        if let Some(n) = &self.proc.notice {
            ui.label(RichText::new(format!("✔ {n}")).color(OK_GREEN).size(12.0));
        }
        if let Some(f) = self.proc.last_out.clone() {
            ui.horizontal(|ui| {
                if ui.small_button("播放").clicked() {
                    let _ = std::process::Command::new("explorer").arg(&f).spawn();
                }
                if ui.small_button("在文件夹中显示").clicked() {
                    let _ = std::process::Command::new("explorer").arg(format!("/select,{}", f.display())).spawn();
                }
            });
        }
    }
}
