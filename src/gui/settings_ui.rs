//! 「设置」页：快捷键、录制行为、保存位置、组件状态。

use eframe::egui::{self, RichText};

use super::hotkeys::{Action, Captured, ACTIONS};
use super::{card, App, MUTED, OK_GREEN, WARN};

impl App {
    fn combo_mut(&mut self, action: Action) -> &mut String {
        match action {
            Action::Record => &mut self.s.hk_record,
            Action::Pause => &mut self.s.hk_pause,
            Action::Shot => &mut self.s.hk_shot,
        }
    }

    /// 按当前设置（重新）注册全部快捷键。
    pub(crate) fn apply_hotkeys(&mut self) {
        self.hotkeys.apply([&self.s.hk_record, &self.s.hk_pause, &self.s.hk_shot]);
    }

    /// 改键状态下每帧调用：等用户按下新的组合键。
    pub(crate) fn poll_hotkey_capture(&mut self, ctx: &egui::Context, on_settings_tab: bool) {
        let Some(action) = self.hotkeys.capturing else { return };
        let captured = if on_settings_tab { self.hotkeys.poll_capture(ctx) } else { Captured::Cancelled };
        match captured {
            Captured::Waiting => return,
            Captured::Rejected(msg) => {
                self.hk_msg = Some(msg);
                return;
            }
            Captured::Cancelled => {}
            Captured::Combo(combo) => *self.combo_mut(action) = combo,
        }
        self.hk_msg = None;
        self.hotkeys.capturing = None;
        self.apply_hotkeys();
    }

    pub(crate) fn ui_settings(&mut self, ui: &mut egui::Ui, recording: bool) {
        ui.add_enabled_ui(!recording, |ui| self.settings_hotkeys(ui));
        ui.add_space(8.0);
        card(ui, "录制", |ui| {
            ui.checkbox(&mut self.s.minimize_on_start, "开始录制后最小化窗口");
            ui.horizontal(|ui| {
                ui.label("定时停止");
                ui.add(egui::DragValue::new(&mut self.s.auto_stop_min).range(0..=1440).suffix(" 分钟"));
                ui.label(RichText::new("0 = 不限时，手动停止").color(MUTED).size(12.0));
            });
        });
        ui.add_space(8.0);
        card(ui, "保存位置（录像与截图）", |ui| {
            ui.label(self.s.output_dir.display().to_string());
            ui.horizontal(|ui| {
                if ui.button("选择文件夹…").clicked() {
                    if let Some(d) = rfd::FileDialog::new().set_directory(&self.s.output_dir).pick_folder() {
                        self.s.output_dir = d;
                    }
                }
                if ui.button("打开文件夹").clicked() {
                    self.open_output_dir();
                }
            });
        });
        ui.add_space(8.0);
        card(ui, "组件", |ui| {
            let row = |ui: &mut egui::Ui, name: &str, state: Result<String, String>| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(name);
                    match state {
                        Ok(s) => ui.label(RichText::new(s).color(OK_GREEN).size(12.0)),
                        Err(e) => ui.label(RichText::new(e).color(WARN).size(12.0)),
                    };
                });
            };
            row(ui, "ffmpeg（录制 / 处理）", self.ffmpeg.as_ref().map(|p| p.display().to_string()).map_err(Clone::clone));
            row(ui, "RIFE（插帧）", self.rife.as_ref().map(|p| p.display().to_string()).map_err(Clone::clone));
            ui.label(RichText::new(format!("screenrec {}", env!("CARGO_PKG_VERSION"))).color(MUTED).size(12.0));
        });
    }

    fn settings_hotkeys(&mut self, ui: &mut egui::Ui) {
        let mut begin: Option<Action> = None;
        let mut clear: Option<Action> = None;
        let mut reset = false;
        card(ui, "快捷键（全局生效，游戏里也能用）", |ui| {
            for a in ACTIONS {
                let capturing = self.hotkeys.capturing == Some(a);
                let combo = self.combo_mut(a).clone();
                ui.horizontal(|ui| {
                    ui.add_sized([120.0, 24.0], egui::Label::new(a.label()));
                    let text = if capturing {
                        "请按下快捷键…（Esc 取消）".to_string()
                    } else if combo.is_empty() {
                        "未设置".to_string()
                    } else {
                        combo.clone()
                    };
                    if ui.add_sized([190.0, 24.0], egui::Button::new(text).selected(capturing)).clicked() {
                        begin = Some(a);
                    }
                    if ui.add_enabled(!combo.is_empty(), egui::Button::new("清除")).clicked() {
                        clear = Some(a);
                    }
                });
                if let Some(e) = &self.hotkeys.errors[a.idx()] {
                    ui.label(RichText::new(e).color(WARN).size(12.0));
                }
            }
            if let Some(m) = &self.hk_msg {
                ui.label(RichText::new(m).color(WARN).size(12.0));
            }
            if let Some(e) = &self.hotkeys.general_err {
                ui.label(RichText::new(e).color(WARN).size(12.0));
            }
            ui.horizontal(|ui| {
                if ui.button("恢复默认").clicked() {
                    reset = true;
                }
                ui.label(
                    RichText::new("点按键框后按下新的组合键；F1~F12 可单独使用，其他键需搭配 Ctrl 或 Alt")
                        .color(MUTED)
                        .size(12.0),
                );
            });
        });
        if let Some(a) = begin {
            self.hk_msg = None;
            self.hotkeys.begin_capture(a);
        }
        if let Some(a) = clear {
            self.combo_mut(a).clear();
            self.hotkeys.capturing = None;
            self.apply_hotkeys();
        }
        if reset {
            let d = super::Settings::default();
            self.s.hk_record = d.hk_record;
            self.s.hk_pause = d.hk_pause;
            self.s.hk_shot = d.hk_shot;
            self.hotkeys.capturing = None;
            self.hk_msg = None;
            self.apply_hotkeys();
        }
    }
}
