//! 录制时钟：已录制时长 = 墙钟时间 − 暂停时间。
//! 视频与音频配速线程共用同一个时钟，暂停时所有轨道同时停止前进，继续后无缝衔接。

use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Inner {
    start: Option<Instant>,
    paused_at: Option<Instant>,
    paused_total: Duration,
}

#[derive(Default)]
pub struct Clock {
    inner: Mutex<Inner>,
}

impl Clock {
    /// 开始计时（录制正式开始的时刻）。
    pub fn start(&self) {
        let mut c = self.inner.lock().unwrap();
        c.start = Some(Instant::now());
        c.paused_at = None;
        c.paused_total = Duration::ZERO;
    }

    /// 已录制的秒数（不含暂停）；尚未开始时为 0。
    pub fn elapsed(&self) -> f64 {
        let c = self.inner.lock().unwrap();
        match c.start {
            Some(s) => {
                let now = c.paused_at.unwrap_or_else(Instant::now);
                now.duration_since(s).saturating_sub(c.paused_total).as_secs_f64()
            }
            None => 0.0,
        }
    }

    pub fn is_paused(&self) -> bool {
        self.inner.lock().unwrap().paused_at.is_some()
    }

    pub fn set_paused(&self, paused: bool) {
        let mut c = self.inner.lock().unwrap();
        if c.start.is_none() {
            return;
        }
        match (paused, c.paused_at) {
            (true, None) => c.paused_at = Some(Instant::now()),
            (false, Some(at)) => {
                c.paused_total += at.elapsed();
                c.paused_at = None;
            }
            _ => {}
        }
    }
}
