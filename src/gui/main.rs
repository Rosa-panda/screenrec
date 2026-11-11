//! screenrec 图形界面（egui/eframe）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod hotkeys;
mod process_ui;
mod settings_ui;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, RichText, Sense, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use windows_capture::monitor::Monitor;
use windows_capture::window::Window;

use screenrec::audio::{self, AudioSource};
use screenrec::encoder::{self, Backend, Codec};
use screenrec::process;
use screenrec::recorder::{Engine, Phase, RecordConfig, Session};
use screenrec::video::Target;

use hotkeys::{Hotkeys, ShotCfg};
use process_ui::{ProcSettings, ProcUi};

// ---------- 配色 ----------
const BG: Color32 = Color32::from_rgb(24, 24, 32);
const CARD: Color32 = Color32::from_rgb(34, 34, 46);
const CARD_STROKE: Color32 = Color32::from_rgb(48, 48, 64);
const TEXT: Color32 = Color32::from_rgb(228, 228, 236);
const MUTED: Color32 = Color32::from_rgb(140, 140, 160);
const ACCENT: Color32 = Color32::from_rgb(255, 59, 74);
const ACCENT_DIM: Color32 = Color32::from_rgb(120, 40, 48);
const OK_GREEN: Color32 = Color32::from_rgb(76, 200, 120);
const WARN: Color32 = Color32::from_rgb(255, 180, 60);

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum SourceKind {
    Monitor,
    Window,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Preset {
    Smooth,
    Hd,
    Ultra,
    Custom,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Record,
    Process,
    Settings,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    source: SourceKind,
    monitor_index: usize,
    window_title: String,
    cursor: bool,
    system_audio: bool,
    system_device: String,
    mic: bool,
    mic_device: String,
    system_volume: f32,
    mic_volume: f32,
    separate_tracks: bool,
    preset: Preset,
    codec: Codec,
    backend: Backend,
    fps: u32,
    quality: u32,
    audio_bitrate: u32,
    output_dir: PathBuf,
    minimize_on_start: bool,
    show_advanced: bool,
    engine: Engine,
    /// 快捷键（如 "F9"、"Ctrl+Shift+R"；空 = 不设置）
    hk_record: String,
    hk_pause: String,
    hk_shot: String,
    /// 定时停止（分钟），0 = 不限时
    auto_stop_min: u32,
    proc: ProcSettings,
}

impl Default for Settings {
    fn default() -> Self {
        let videos = std::env::var("USERPROFILE")
            .map(|p| PathBuf::from(p).join("Videos").join("screenrec"))
            .unwrap_or_else(|_| PathBuf::from("."));
        Self {
            source: SourceKind::Monitor,
            monitor_index: 1,
            window_title: String::new(),
            cursor: true,
            system_audio: true,
            system_device: String::new(),
            mic: true,
            mic_device: String::new(),
            system_volume: 1.0,
            mic_volume: 1.0,
            separate_tracks: false,
            preset: Preset::Hd,
            codec: Codec::H264,
            backend: Backend::Auto,
            fps: 60,
            quality: 18,
            audio_bitrate: 256,
            output_dir: videos,
            minimize_on_start: true,
            show_advanced: false,
            engine: Engine::Standard,
            hk_record: "F9".into(),
            hk_pause: "F10".into(),
            hk_shot: "F8".into(),
            auto_stop_min: 0,
            proc: ProcSettings::default(),
        }
    }
}

struct MonitorInfo {
    index: usize,
    label: String,
}

struct App {
    s: Settings,
    host: cpal::Host,
    monitors: Vec<MonitorInfo>,
    windows: Vec<String>,
    out_devices: Vec<String>,
    in_devices: Vec<String>,
    sys_src: Option<AudioSource>,
    sys_err: Option<String>,
    mic_src: Option<AudioSource>,
    mic_err: Option<String>,
    sys_key: String,
    mic_key: String,
    sys_level: f32,
    mic_level: f32,
    ffmpeg: Result<PathBuf, String>,
    session: Option<Session>,
    last_file: Option<PathBuf>,
    last_error: Option<String>,
    notice: Option<String>,
    tab: Tab,
    hotkeys: Hotkeys,
    /// 改键时的提示（按键不合适等）
    hk_msg: Option<String>,
    rife: Result<PathBuf, String>,
    proc: ProcUi,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_style(&cc.egui_ctx);
        let s: Settings = cc
            .storage
            .and_then(|st| eframe::get_value(st, eframe::APP_KEY))
            .unwrap_or_default();
        let host = cpal::default_host();
        let ffmpeg = encoder::find_ffmpeg(None).map_err(|e| e.to_string());
        let rife = process::find_rife(None).map_err(|e| e.to_string());

        let mut app = Self {
            s,
            host,
            monitors: Vec::new(),
            windows: Vec::new(),
            out_devices: Vec::new(),
            in_devices: Vec::new(),
            sys_src: None,
            sys_err: None,
            mic_src: None,
            mic_err: None,
            sys_key: "\u{0}".into(),
            mic_key: "\u{0}".into(),
            sys_level: 0.0,
            mic_level: 0.0,
            ffmpeg,
            session: None,
            last_file: None,
            last_error: None,
            notice: None,
            tab: Tab::Record,
            hotkeys: Hotkeys::new(&cc.egui_ctx),
            hk_msg: None,
            rife,
            proc: ProcUi::default(),
        };
        app.apply_hotkeys();
        app.refresh_sources();
        app
    }

    fn open_output_dir(&self) {
        let _ = std::fs::create_dir_all(&self.s.output_dir);
        let _ = std::process::Command::new("explorer").arg(&self.s.output_dir).spawn();
    }

    /// 把截图所需的设置同步给热键线程，并取回上一次截图的结果。
    fn sync_screenshot(&mut self) {
        let shared = &self.hotkeys.shared;
        *shared.shot_cfg.lock().unwrap() = Some(ShotCfg {
            monitor: self.s.source == SourceKind::Monitor,
            monitor_index: self.s.monitor_index,
            window_title: self.s.window_title.clone(),
            cursor: self.s.cursor,
            dir: self.s.output_dir.clone(),
            ffmpeg: self.ffmpeg.as_ref().ok().cloned(),
        });
        let result = shared.shot_result.lock().unwrap().take();
        match result {
            Some(Ok(path)) => {
                self.notice = Some(format!(
                    "截图已保存  {}",
                    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
                ));
                self.last_error = None;
            }
            Some(Err(e)) => self.last_error = Some(e),
            None => {}
        }
    }

    /// 按钮上显示的快捷键提示，如 "   (F9)"；未设置则为空。
    fn hk_hint(combo: &str) -> String {
        if combo.is_empty() {
            String::new()
        } else {
            format!("   ({combo})")
        }
    }

    fn refresh_sources(&mut self) {
        self.monitors = Monitor::enumerate()
            .unwrap_or_default()
            .into_iter()
            .map(|m| {
                let index = m.index().unwrap_or(0);
                let name = m.name().unwrap_or_else(|_| "?".into());
                let (w, h) = (m.width().unwrap_or(0), m.height().unwrap_or(0));
                MonitorInfo { index, label: format!("[{index}] {name}  {w}×{h}") }
            })
            .collect();
        if !self.monitors.iter().any(|m| m.index == self.s.monitor_index) {
            if let Some(m) = self.monitors.first() {
                self.s.monitor_index = m.index;
            }
        }
        self.windows = Window::enumerate()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|w| w.title().ok())
            .filter(|t| !t.trim().is_empty())
            .collect();
        self.out_devices = audio::output_device_names(&self.host);
        self.in_devices = audio::input_device_names(&self.host);
    }

    /// 根据当前设置（开关 + 设备）开关音频流，供电平表与录制共用。
    fn sync_audio(&mut self) {
        let sys_key = if self.s.system_audio { format!("on:{}", self.s.system_device) } else { "off".into() };
        if sys_key != self.sys_key {
            self.sys_key = sys_key;
            self.sys_src = None;
            self.sys_err = None;
            if self.s.system_audio {
                let name = (!self.s.system_device.is_empty()).then_some(self.s.system_device.as_str());
                match audio::open_system_audio(&self.host, name) {
                    Ok(src) => self.sys_src = Some(src),
                    Err(e) => self.sys_err = Some(e.to_string()),
                }
            }
        }
        let mic_key = if self.s.mic { format!("on:{}", self.s.mic_device) } else { "off".into() };
        if mic_key != self.mic_key {
            self.mic_key = mic_key;
            self.mic_src = None;
            self.mic_err = None;
            if self.s.mic {
                let name = (!self.s.mic_device.is_empty()).then_some(self.s.mic_device.as_str());
                match audio::open_mic(&self.host, name) {
                    Ok(src) => self.mic_src = Some(src),
                    Err(e) => self.mic_err = Some(e.to_string()),
                }
            }
        }
    }

    fn is_recording(&self) -> bool {
        self.session.is_some()
    }

    fn apply_preset(&mut self) {
        match self.s.preset {
            Preset::Smooth => {
                self.s.fps = 30;
                self.s.quality = 23;
            }
            Preset::Hd => {
                self.s.fps = 60;
                self.s.quality = 18;
            }
            Preset::Ultra => {
                self.s.fps = 60;
                self.s.quality = 15;
            }
            Preset::Custom => {}
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        self.last_error = None;
        let ffmpeg = match &self.ffmpeg {
            Ok(p) => p.clone(),
            Err(e) => {
                self.last_error = Some(e.clone());
                return;
            }
        };
        let target = match self.s.source {
            SourceKind::Monitor => match Monitor::from_index(self.s.monitor_index) {
                Ok(m) => Target::Monitor(m),
                Err(e) => {
                    self.last_error = Some(format!("显示器不可用: {e:?}"));
                    return;
                }
            },
            SourceKind::Window => {
                if self.s.window_title.trim().is_empty() {
                    self.last_error = Some("请先选择要录制的窗口".into());
                    return;
                }
                match Window::from_contains_name(&self.s.window_title) {
                    Ok(w) => Target::Window(w),
                    Err(e) => {
                        self.last_error = Some(format!("找不到窗口 \"{}\": {e:?}", self.s.window_title));
                        return;
                    }
                }
            }
        };
        let mut inputs = Vec::new();
        if let Some(src) = &self.sys_src {
            inputs.push(src.input(self.s.system_volume));
        }
        if let Some(src) = &self.mic_src {
            inputs.push(src.input(self.s.mic_volume));
        }
        let output = self
            .s
            .output_dir
            .join(format!("rec_{}.mp4", chrono::Local::now().format("%Y%m%d_%H%M%S")));
        let cfg = RecordConfig {
            output,
            target,
            cursor: self.s.cursor,
            fps: self.s.fps,
            codec: self.s.codec,
            backend: self.s.backend,
            quality: self.s.quality,
            audio_bitrate: self.s.audio_bitrate,
            separate_tracks: self.s.separate_tracks,
            audio: inputs,
            duration: (self.s.auto_stop_min > 0).then(|| self.s.auto_stop_min as f64 * 60.0),
            ffmpeg,
            engine: self.s.engine,
            hide_console: true,
            echo_ffmpeg: false,
        };
        let session = Session::start(cfg);
        *self.hotkeys.shared.session.lock().unwrap() = Some(session.state.clone());
        self.session = Some(session);
        if self.s.minimize_on_start {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
    }

    fn stop(&mut self) {
        if let Some(s) = &self.session {
            s.stop();
        }
    }

    fn poll_session(&mut self, ctx: &egui::Context) {
        let finished = self.session.as_ref().map(|s| s.is_finished()).unwrap_or(false);
        if !finished {
            return;
        }
        let session = self.session.take().unwrap();
        *self.hotkeys.shared.session.lock().unwrap() = None;
        let st = session.state.clone();
        match session.wait() {
            Ok(()) => {
                self.last_file = Some(st.output.clone());
                self.notice = Some(format!(
                    "已保存  {}  （{}，{:.1} MB）",
                    st.output.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    fmt_time(st.elapsed()),
                    st.size.load(Ordering::Relaxed) as f64 / 1048576.0
                ));
            }
            Err(e) => self.last_error = Some(e),
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    }

    fn poll_hotkey(&mut self, ctx: &egui::Context) {
        // 热键在独立线程里处理（见 hotkeys.rs）；这里只响应它发来的“开始”请求
        if self.hotkeys.shared.start_requested.swap(false, Ordering::Relaxed) && !self.is_recording() {
            self.start(ctx);
        }
    }
}

fn fmt_time(secs: f64) -> String {
    let s = secs as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

fn setup_style(ctx: &egui::Context) {
    // 中文字体：微软雅黑（ttc 第 0 个字体）
    let mut fonts = egui::FontDefinitions::default();
    for path in ["C:\\Windows\\Fonts\\msyh.ttc", "C:\\Windows\\Fonts\\msyhl.ttc", "C:\\Windows\\Fonts\\simhei.ttf"] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert("cjk".into(), Arc::new(egui::FontData::from_owned(bytes)));
            fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "cjk".into());
            fonts.families.entry(egui::FontFamily::Monospace).or_default().push("cjk".into());
            break;
        }
    }
    ctx.set_fonts(fonts);

    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT_DIM;
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.widgets.inactive.bg_fill = Color32::from_rgb(46, 46, 62);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(46, 46, 62);
    v.widgets.hovered.bg_fill = Color32::from_rgb(58, 58, 78);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(58, 58, 78);
    v.widgets.active.bg_fill = Color32::from_rgb(70, 70, 94);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(70, 70, 94);
    for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = CornerRadius::same(6);
    }
    ctx.set_visuals(v);
    ctx.all_styles_mut(|st| {
        st.spacing.item_spacing = Vec2::new(8.0, 8.0);
        st.spacing.button_padding = Vec2::new(10.0, 5.0);
        st.spacing.slider_width = 140.0;
        st.text_styles.insert(egui::TextStyle::Body, FontId::proportional(14.0));
        st.text_styles.insert(egui::TextStyle::Button, FontId::proportional(14.0));
        st.text_styles.insert(egui::TextStyle::Small, FontId::proportional(12.0));
        st.text_styles.insert(egui::TextStyle::Heading, FontId::proportional(18.0));
    });
}

fn card<R>(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, CARD_STROKE))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).color(MUTED).size(12.0).strong());
            ui.add_space(4.0);
            add(ui)
        })
        .inner
}

fn level_meter(ui: &mut egui::Ui, level: f32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 8.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(4), Color32::from_rgb(50, 50, 66));
    let l = level.clamp(0.0, 1.0);
    if l > 0.0 {
        let mut r = rect;
        r.set_width(rect.width() * l);
        let color = if l > 0.9 { ACCENT } else if l > 0.6 { WARN } else { OK_GREEN };
        p.rect_filled(r, CornerRadius::same(4), color);
    }
}

fn big_button(ui: &mut egui::Ui, text: &str, color: Color32, enabled: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 50.0), Sense::click());
    let fill = if !enabled {
        Color32::from_rgb(60, 60, 76)
    } else if resp.hovered() {
        color.gamma_multiply(1.15)
    } else {
        color
    };
    ui.painter().rect_filled(rect, CornerRadius::same(25), fill);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, FontId::proportional(17.0), Color32::WHITE);
    enabled && resp.clicked()
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.s);
    }

    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &root.ctx().clone();
        self.poll_session(ctx);
        self.poll_hotkey(ctx);
        self.poll_hotkey_capture(ctx, self.tab == Tab::Settings);
        self.sync_screenshot();
        if !self.is_recording() {
            self.sync_audio();
        }
        // 把视频文件拖进窗口 = 送去「视频处理」
        let dropped = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()));
        if let Some(path) = dropped {
            if !self.proc.busy() {
                self.set_proc_src(path);
                self.tab = Tab::Process;
            }
        }
        // 电平表：取峰值并带衰减
        let decay = 0.85;
        let sys = self.sys_src.as_ref().map(|s| s.take_level()).unwrap_or(0.0);
        let mic = self.mic_src.as_ref().map(|s| s.take_level()).unwrap_or(0.0);
        self.sys_level = (self.sys_level * decay).max(sys);
        self.mic_level = (self.mic_level * decay).max(mic);
        ctx.request_repaint_after(Duration::from_millis(66));

        let recording = self.is_recording();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::same(16)))
            .show(root, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    self.ui_header(ui);
                    ui.add_space(6.0);
                    match self.tab {
                        Tab::Record => {
                            self.ui_record_area(ui, ctx, recording);
                            ui.add_space(10.0);
                            ui.add_enabled_ui(!recording, |ui| {
                                self.ui_source(ui);
                                ui.add_space(8.0);
                                self.ui_audio(ui);
                                ui.add_space(8.0);
                                self.ui_quality(ui);
                            });
                            ui.add_space(8.0);
                            self.ui_footer(ui);
                        }
                        Tab::Process => self.ui_process(ui),
                        Tab::Settings => self.ui_settings(ui, recording),
                    }
                });
            });
    }
}

impl App {
    fn ui_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
            let color = if self.is_recording() { ACCENT } else { Color32::from_rgb(90, 90, 110) };
            ui.painter().circle_filled(rect.center(), 6.0, color);
            ui.label(RichText::new("screenrec").size(20.0).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let paused = self.session.as_ref().is_some_and(|s| s.state.is_paused());
                let status = match self.session.as_ref().map(|s| s.state.phase()) {
                    None => "就绪".to_string(),
                    Some(Phase::Starting) => "准备中…".into(),
                    Some(Phase::Recording) if paused => "已暂停".into(),
                    Some(Phase::Recording) => "录制中".into(),
                    Some(Phase::Finishing) => "正在收尾…".into(),
                    Some(_) => "完成".into(),
                };
                ui.label(RichText::new(status).color(MUTED));
            });
        });
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Record, "   录制   ");
            ui.selectable_value(&mut self.tab, Tab::Process, "   视频处理   ");
            ui.selectable_value(&mut self.tab, Tab::Settings, "   设置   ");
        });
    }

    fn ui_record_area(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, recording: bool) {
        let (elapsed, size, sent, enc) = match &self.session {
            Some(s) => (
                s.state.elapsed(),
                s.state.size.load(Ordering::Relaxed),
                s.state.sent.load(Ordering::Relaxed),
                s.state.encoder_name.lock().unwrap().clone(),
            ),
            None => (0.0, 0, 0, String::new()),
        };
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(fmt_time(elapsed)).font(FontId::monospace(44.0)).color(if recording { TEXT } else { MUTED }));
            let sub = if recording {
                format!("{:.1} MB · {} 帧 · {}", size as f64 / 1048576.0, sent, enc)
            } else {
                format!("{} fps · CQ {} · {:?}", self.s.fps, self.s.quality, self.s.codec)
            };
            ui.label(RichText::new(sub).color(MUTED).size(12.0));
        });
        ui.add_space(8.0);
        let phase = self.session.as_ref().map(|s| s.state.phase());
        let rec_hint = Self::hk_hint(&self.s.hk_record);
        match phase {
            None => {
                if big_button(ui, &format!("●  开始录制{rec_hint}"), ACCENT, self.ffmpeg.is_ok()) {
                    self.start(ctx);
                }
            }
            Some(Phase::Finishing) => {
                big_button(ui, "正在写入文件…", Color32::from_rgb(60, 60, 76), false);
            }
            Some(phase) => {
                let paused = self.session.as_ref().is_some_and(|s| s.state.is_paused());
                let pause_hint = Self::hk_hint(&self.s.hk_pause);
                ui.columns(2, |cols| {
                    if big_button(&mut cols[0], &format!("■  停止{rec_hint}"), Color32::from_rgb(70, 70, 94), true) {
                        self.stop();
                    }
                    let (text, color) = if paused {
                        (format!("▶  继续{pause_hint}"), OK_GREEN.gamma_multiply(0.7))
                    } else {
                        (format!("⏸  暂停{pause_hint}"), Color32::from_rgb(70, 70, 94))
                    };
                    if big_button(&mut cols[1], &text, color, phase == Phase::Recording) {
                        if let Some(s) = &self.session {
                            s.state.toggle_pause();
                        }
                    }
                });
            }
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button(format!("截图{}", Self::hk_hint(&self.s.hk_shot))).clicked() {
                hotkeys::take_screenshot(self.hotkeys.shared.clone(), ctx.clone());
            }
            if ui.button("打开保存文件夹").clicked() {
                self.open_output_dir();
            }
            if let Some(e) = self.hotkeys.errors.iter().flatten().next().or(self.hotkeys.general_err.as_ref()) {
                ui.label(RichText::new(e).color(WARN).size(12.0));
            }
        });
    }

    fn ui_source(&mut self, ui: &mut egui::Ui) {
        card(ui, "录制来源", |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.s.source, SourceKind::Monitor, "  显示器  ");
                ui.selectable_value(&mut self.s.source, SourceKind::Window, "  窗口  ");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⟳ 刷新").clicked() {
                        self.refresh_sources();
                    }
                });
            });
            match self.s.source {
                SourceKind::Monitor => {
                    let current = self
                        .monitors
                        .iter()
                        .find(|m| m.index == self.s.monitor_index)
                        .map(|m| m.label.clone())
                        .unwrap_or_else(|| "（没有显示器）".into());
                    egui::ComboBox::from_id_salt("monitor")
                        .width(ui.available_width())
                        .selected_text(current)
                        .show_ui(ui, |ui| {
                            for m in &self.monitors {
                                ui.selectable_value(&mut self.s.monitor_index, m.index, &m.label);
                            }
                        });
                }
                SourceKind::Window => {
                    let current = if self.s.window_title.is_empty() {
                        "选择窗口…".to_string()
                    } else {
                        self.s.window_title.clone()
                    };
                    egui::ComboBox::from_id_salt("window")
                        .width(ui.available_width())
                        .selected_text(current)
                        .show_ui(ui, |ui| {
                            for t in &self.windows {
                                ui.selectable_value(&mut self.s.window_title, t.clone(), t);
                            }
                        });
                }
            }
            ui.checkbox(&mut self.s.cursor, "录制鼠标指针");
        });
    }

    fn ui_audio(&mut self, ui: &mut egui::Ui) {
        card(ui, "音频", |ui| {
            // 系统声音
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.s.system_audio, "系统声音");
                ui.add_enabled_ui(self.s.system_audio, |ui| {
                    let current = if self.s.system_device.is_empty() {
                        "默认输出设备".to_string()
                    } else {
                        self.s.system_device.clone()
                    };
                    egui::ComboBox::from_id_salt("sysdev")
                        .width(200.0)
                        .selected_text(current)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.s.system_device, String::new(), "默认输出设备");
                            for d in &self.out_devices {
                                ui.selectable_value(&mut self.s.system_device, d.clone(), d);
                            }
                        });
                });
            });
            ui.horizontal(|ui| {
                ui.add_space(22.0);
                level_meter(ui, self.sys_level, 150.0);
                ui.add_enabled(
                    self.s.system_audio,
                    egui::Slider::new(&mut self.s.system_volume, 0.0..=2.0).fixed_decimals(1).text("音量"),
                );
            });
            if let (true, Some(e)) = (self.s.system_audio, &self.sys_err) {
                ui.label(RichText::new(short_err(e)).color(WARN).size(12.0));
            }
            ui.add_space(4.0);
            // 麦克风
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.s.mic, "麦克风    ");
                ui.add_enabled_ui(self.s.mic, |ui| {
                    let current = if self.s.mic_device.is_empty() {
                        "默认麦克风".to_string()
                    } else {
                        self.s.mic_device.clone()
                    };
                    egui::ComboBox::from_id_salt("micdev")
                        .width(200.0)
                        .selected_text(current)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.s.mic_device, String::new(), "默认麦克风");
                            for d in &self.in_devices {
                                ui.selectable_value(&mut self.s.mic_device, d.clone(), d);
                            }
                        });
                });
            });
            ui.horizontal(|ui| {
                ui.add_space(22.0);
                level_meter(ui, self.mic_level, 150.0);
                ui.add_enabled(
                    self.s.mic,
                    egui::Slider::new(&mut self.s.mic_volume, 0.0..=3.0).fixed_decimals(1).text("音量"),
                );
            });
            if let (true, Some(e)) = (self.s.mic, &self.mic_err) {
                ui.label(RichText::new(short_err(e)).color(WARN).size(12.0));
            }
            ui.add_space(2.0);
            ui.checkbox(&mut self.s.separate_tracks, "系统声音与麦克风分成两条音轨");
        });
    }

    fn ui_quality(&mut self, ui: &mut egui::Ui) {
        card(ui, "画质", |ui| {
            ui.horizontal(|ui| {
                let before = self.s.preset;
                ui.selectable_value(&mut self.s.preset, Preset::Smooth, "  流畅 30fps  ");
                ui.selectable_value(&mut self.s.preset, Preset::Hd, "  高清 60fps  ");
                ui.selectable_value(&mut self.s.preset, Preset::Ultra, "  极致  ");
                ui.selectable_value(&mut self.s.preset, Preset::Custom, "  自定义  ");
                if before != self.s.preset {
                    self.apply_preset();
                }
            });
            ui.horizontal(|ui| {
                ui.label("引擎");
                ui.selectable_value(&mut self.s.engine, Engine::Standard, "  标准  ");
                ui.selectable_value(&mut self.s.engine, Engine::Game, "  游戏  ");
                ui.selectable_value(&mut self.s.engine, Engine::ZeroCopy, "  零拷贝  ");
            });
            let hint = match self.s.engine {
                Engine::Standard => "标准：画质与兼容性最好，录桌面/教程用",
                Engine::Game => "游戏：轻量编码预设，录制让出 CPU/GPU 优先级；保留 CQ 画质、AV1、分轨等全部功能",
                Engine::ZeroCopy => "零拷贝：画面不经过 CPU，占用最低；仅 H.264/HEVC，按码率控制画质，音轨固定混合",
            };
            ui.label(RichText::new(hint).color(MUTED).size(12.0));
            ui.horizontal(|ui| {
                ui.label("编码");
                egui::ComboBox::from_id_salt("codec")
                    .width(90.0)
                    .selected_text(codec_name(self.s.codec))
                    .show_ui(ui, |ui| {
                        for c in [Codec::H264, Codec::Hevc, Codec::Av1] {
                            ui.selectable_value(&mut self.s.codec, c, codec_name(c));
                        }
                    });
                ui.add_space(8.0);
                ui.label("后端");
                egui::ComboBox::from_id_salt("backend")
                    .width(90.0)
                    .selected_text(backend_name(self.s.backend))
                    .show_ui(ui, |ui| {
                        for b in [Backend::Auto, Backend::Nvenc, Backend::Qsv, Backend::Amf, Backend::Cpu] {
                            ui.selectable_value(&mut self.s.backend, b, backend_name(b));
                        }
                    });
            });
            ui.add_enabled_ui(self.s.preset == Preset::Custom, |ui| {
                ui.horizontal(|ui| {
                    ui.label("帧率");
                    ui.add(egui::Slider::new(&mut self.s.fps, 15..=144));
                    ui.add_space(8.0);
                    ui.label("画质 CQ");
                    ui.add(egui::Slider::new(&mut self.s.quality, 10..=30));
                });
            });
            ui.label(RichText::new("CQ 越小画质越高、文件越大；18 接近无损观感，23 文件小一半").color(MUTED).size(12.0));
        });
    }

    fn ui_footer(&mut self, ui: &mut egui::Ui) {
        if let Some(e) = &self.last_error {
            let first = e.lines().next().unwrap_or("").to_string();
            ui.label(RichText::new(format!("✖ {first}")).color(ACCENT));
            if e.lines().count() > 1 {
                ui.collapsing("详情", |ui| {
                    ui.label(RichText::new(e.clone()).size(12.0).color(MUTED));
                });
            }
        }
        if let Some(n) = &self.notice {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("✔ {n}")).color(OK_GREEN).size(12.0));
            });
        }
        if let Some(f) = self.last_file.clone() {
            ui.horizontal(|ui| {
                if ui.small_button("播放").clicked() {
                    let _ = std::process::Command::new("explorer").arg(&f).spawn();
                }
                if ui.small_button("在文件夹中显示").clicked() {
                    let _ = std::process::Command::new("explorer").arg(format!("/select,{}", f.display())).spawn();
                }
                if !self.proc.busy() && ui.small_button("插帧 / 处理此视频").clicked() {
                    self.set_proc_src(f.clone());
                    self.tab = Tab::Process;
                }
            });
        }
        if let Err(e) = &self.ffmpeg {
            ui.label(RichText::new(e).color(ACCENT).size(12.0));
        }
    }
}

/// 调试/崩溃日志：%LOCALAPPDATA%\screenrec\gui.log
fn debug_log(msg: &str) {
    if let Ok(base) = std::env::var("LOCALAPPDATA") {
        let dir = PathBuf::from(base).join("screenrec");
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("gui.log")) {
            use std::io::Write;
            let _ = writeln!(f, "{} {msg}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
        }
    }
}

fn short_err(e: &str) -> String {
    if e.contains("Access is denied") || e.contains("拒绝访问") {
        "不可用：Windows 隐私设置禁止了麦克风访问（设置 > 隐私和安全性 > 麦克风）".into()
    } else {
        format!("不可用：{}", e.lines().next().unwrap_or(""))
    }
}

fn codec_name(c: Codec) -> &'static str {
    match c {
        Codec::H264 => "H.264",
        Codec::Hevc => "HEVC",
        Codec::Av1 => "AV1",
    }
}

fn backend_name(b: Backend) -> &'static str {
    match b {
        Backend::Auto => "自动",
        Backend::Nvenc => "NVENC",
        Backend::Qsv => "QSV",
        Backend::Amf => "AMF",
        Backend::Cpu => "CPU",
    }
}

/// 程序图标：与 assets/icon.png 同款，程序化绘制，免去图片解码依赖。
fn make_icon() -> egui::IconData {
    let n = 64u32;
    let c = n as f32 / 2.0;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
            let d = (fx * fx + fy * fy).sqrt();
            let half = c;
            let r_corner = half * 0.44;
            let qx = (fx.abs() - (half - r_corner)).max(0.0);
            let qy = (fy.abs() - (half - r_corner)).max(0.0);
            let inside = (qx * qx + qy * qy).sqrt() <= r_corner;
            let px: [u8; 4] = if !inside {
                [0, 0, 0, 0]
            } else if d < half * 0.66 {
                [255, 59, 74, 255]
            } else if d >= half * 0.77 && d < half * 0.875 {
                [240, 240, 245, 255]
            } else {
                [30, 30, 46, 255]
            };
            rgba.extend_from_slice(&px);
        }
    }
    egui::IconData { rgba, width: n, height: n }
}

fn main() -> eframe::Result {
    std::panic::set_hook(Box::new(|info| debug_log(&format!("PANIC: {info}"))));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("screenrec 录屏")
            .with_inner_size([470.0, 760.0])
            .with_min_inner_size([440.0, 600.0])
            .with_icon(make_icon()),
        centered: true,
        ..Default::default()
    };
    eframe::run_native("screenrec", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
