//! 全局快捷键：注册 / 改键 / 响应。
//!
//! 响应放在独立线程里：窗口最小化时界面循环不运行，停止、暂停、截图都不能依赖界面。
//! 只有「开始录制」需要界面线程的设置与音频流，所以先唤醒窗口再由界面线程执行。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Key};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use windows_capture::monitor::Monitor;
use windows_capture::window::Window;

use screenrec::recorder::{Phase, SessionState};
use screenrec::screenshot;
use screenrec::video::Target;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Record,
    Pause,
    Shot,
}

pub const ACTIONS: [Action; 3] = [Action::Record, Action::Pause, Action::Shot];

impl Action {
    pub fn idx(self) -> usize {
        self as usize
    }
    pub fn label(self) -> &'static str {
        match self {
            Action::Record => "开始 / 停止录制",
            Action::Pause => "暂停 / 继续",
            Action::Shot => "截图",
        }
    }
}

/// 截图需要的全部信息（界面线程每帧更新，热键线程直接取用）。
#[derive(Clone)]
pub struct ShotCfg {
    pub monitor: bool,
    pub monitor_index: usize,
    pub window_title: String,
    pub cursor: bool,
    pub dir: PathBuf,
    pub ffmpeg: Option<PathBuf>,
}

/// 热键线程与界面线程共享的状态。
#[derive(Default)]
pub struct Shared {
    /// 当前录制会话（界面线程在开始/结束时更新）
    pub session: Mutex<Option<Arc<SessionState>>>,
    /// 热键线程请求界面线程开始录制
    pub start_requested: AtomicBool,
    ids: Mutex<[Option<u32>; 3]>,
    pub shot_cfg: Mutex<Option<ShotCfg>>,
    pub shot_result: Mutex<Option<Result<PathBuf, String>>>,
}

pub enum Captured {
    Waiting,
    Cancelled,
    Combo(String),
    Rejected(String),
}

pub struct Hotkeys {
    mgr: Option<GlobalHotKeyManager>,
    registered: Vec<HotKey>,
    pub shared: Arc<Shared>,
    /// 每个动作的注册错误
    pub errors: [Option<String>; 3],
    pub general_err: Option<String>,
    /// 正在等待用户按下新快捷键的动作
    pub capturing: Option<Action>,
}

impl Hotkeys {
    pub fn new(ctx: &egui::Context) -> Self {
        let shared = Arc::new(Shared::default());
        let (mgr, general_err) = match GlobalHotKeyManager::new() {
            Ok(m) => {
                spawn_thread(shared.clone(), ctx.clone());
                (Some(m), None)
            }
            Err(e) => (None, Some(format!("快捷键不可用: {e}"))),
        };
        Self { mgr, registered: Vec::new(), shared, errors: Default::default(), general_err, capturing: None }
    }

    fn unregister_all(&mut self) {
        if let Some(mgr) = &self.mgr {
            let _ = mgr.unregister_all(&self.registered);
        }
        self.registered.clear();
        *self.shared.ids.lock().unwrap() = [None; 3];
    }

    /// 按设置重新注册全部快捷键（顺序同 ACTIONS；空字符串表示不设置）。
    pub fn apply(&mut self, combos: [&str; 3]) {
        self.unregister_all();
        self.errors = Default::default();
        let Some(mgr) = &self.mgr else { return };
        let mut ids = [None; 3];
        for a in ACTIONS {
            let i = a.idx();
            let s = combos[i].trim();
            if s.is_empty() {
                continue;
            }
            let Ok(hk) = s.parse::<HotKey>() else {
                self.errors[i] = Some(format!("无法识别的快捷键: {s}"));
                continue;
            };
            if self.registered.iter().any(|r| r.id() == hk.id()) {
                self.errors[i] = Some(format!("{s} 与其他功能的快捷键重复"));
                continue;
            }
            match mgr.register(hk) {
                Ok(()) => {
                    ids[i] = Some(hk.id());
                    self.registered.push(hk);
                }
                Err(_) => self.errors[i] = Some(format!("{s} 注册失败（可能已被其他程序占用）")),
            }
        }
        *self.shared.ids.lock().unwrap() = ids;
    }

    /// 进入改键状态：先注销全部快捷键，否则按键会被系统截走，窗口收不到。
    pub fn begin_capture(&mut self, action: Action) {
        self.unregister_all();
        self.capturing = Some(action);
    }

    /// 改键状态下每帧调用：读取用户按下的组合键。
    pub fn poll_capture(&self, ctx: &egui::Context) -> Captured {
        let pressed = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Key { key, pressed: true, repeat: false, modifiers, .. } => Some((*key, *modifiers)),
                _ => None,
            })
        });
        let Some((key, mods)) = pressed else { return Captured::Waiting };
        if key == Key::Escape && mods.is_none() {
            return Captured::Cancelled;
        }
        // egui 的键名与热键库的写法只有这几个不一致
        let name = match key {
            Key::Equals => "Equal",
            Key::OpenBracket => "BracketLeft",
            Key::CloseBracket => "BracketRight",
            Key::Backtick => "Backquote",
            k => k.name(),
        };
        let mut combo = String::new();
        for (on, text) in [(mods.ctrl, "Ctrl+"), (mods.alt, "Alt+"), (mods.shift, "Shift+")] {
            if on {
                combo.push_str(text);
            }
        }
        combo.push_str(name);
        if combo.parse::<HotKey>().is_err() {
            return Captured::Rejected(format!("不支持把 {name} 设为快捷键"));
        }
        let fkey = name.len() >= 2 && name.starts_with('F') && name[1..].chars().all(|c| c.is_ascii_digit());
        let nav = matches!(key, Key::Insert | Key::Home | Key::End | Key::PageUp | Key::PageDown);
        if !(mods.ctrl || mods.alt || fkey || nav) {
            return Captured::Rejected("字母、数字等普通按键需要搭配 Ctrl 或 Alt，否则会影响正常打字".into());
        }
        Captured::Combo(combo)
    }
}

fn spawn_thread(shared: Arc<Shared>, ctx: egui::Context) {
    std::thread::spawn(move || {
        while let Ok(ev) = GlobalHotKeyEvent::receiver().recv() {
            if ev.state != HotKeyState::Pressed {
                continue;
            }
            let action = {
                let ids = shared.ids.lock().unwrap();
                ACTIONS.into_iter().find(|a| ids[a.idx()] == Some(ev.id))
            };
            let Some(action) = action else { continue };
            let active = shared
                .session
                .lock()
                .unwrap()
                .as_ref()
                .filter(|s| !matches!(s.phase(), Phase::Done | Phase::Failed))
                .cloned();
            match action {
                Action::Record => {
                    match active {
                        Some(s) => s.request_stop(),
                        None => shared.start_requested.store(true, Ordering::Relaxed),
                    }
                    wake_window();
                }
                Action::Pause => {
                    if let Some(s) = active {
                        s.toggle_pause();
                    }
                }
                Action::Shot => take_screenshot(shared.clone(), ctx.clone()),
            }
            ctx.request_repaint();
        }
    });
}

/// 对当前录制来源截一张图（后台线程执行，结果放进 `shot_result`）。
pub fn take_screenshot(shared: Arc<Shared>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let result = (|| -> Result<PathBuf, String> {
            let cfg = shared.shot_cfg.lock().unwrap().clone().ok_or("截图设置未就绪")?;
            let ffmpeg = cfg.ffmpeg.ok_or("找不到 ffmpeg，无法保存截图")?;
            let target = if cfg.monitor {
                Target::Monitor(Monitor::from_index(cfg.monitor_index).map_err(|e| format!("显示器不可用: {e:?}"))?)
            } else {
                Target::Window(
                    Window::from_contains_name(&cfg.window_title)
                        .map_err(|e| format!("找不到窗口 \"{}\": {e:?}", cfg.window_title))?,
                )
            };
            let name = format!("shot_{}.png", chrono::Local::now().format("%Y%m%d_%H%M%S_%3f"));
            let out = cfg.dir.join(name);
            screenshot::capture_png(target, cfg.cursor, &ffmpeg, &out).map_err(|e| format!("截图失败: {e}"))?;
            Ok(out)
        })();
        *shared.shot_result.lock().unwrap() = Some(result);
        ctx.request_repaint();
    });
}

/// 窗口最小化时把它恢复出来（不抢焦点），界面循环才会继续运行。
fn wake_window() {
    use windows::core::w;
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, IsIconic, ShowWindow, SW_SHOWNOACTIVATE};
    unsafe {
        if let Ok(hwnd) = FindWindowW(None, w!("screenrec 录屏")) {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
        }
    }
}
