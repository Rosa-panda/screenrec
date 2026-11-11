//! 截图：对录制目标抓一帧，保存为 PNG（PNG 编码交给 ffmpeg，免去图片库依赖）。

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::video::{self, BoxError, FrameStore, Target};

pub fn capture_png(target: Target, cursor: bool, ffmpeg: &Path, out: &Path) -> Result<(u32, u32), BoxError> {
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let store = FrameStore::new(false);
    let control = video::start(target, cursor, 60, store.clone())?;
    let size = store.wait_first_frame(Duration::from_secs(3));
    store.stop.store(true, Ordering::Relaxed);
    let _ = control.stop();
    let (w, h) = size.ok_or("没有抓到画面（窗口是否最小化？）")?;
    let data = std::mem::take(&mut store.slot.lock().unwrap().data);

    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-f", "rawvideo", "-pix_fmt", "bgra"])
        .args(["-video_size", &format!("{w}x{h}"), "-i", "pipe:0", "-frames:v", "1", "-pix_fmt", "rgb24"])
        .arg(out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&data)?;
    }
    if !child.wait()?.success() {
        return Err("截图保存失败（ffmpeg 写 PNG 出错）".into());
    }
    Ok((w, h))
}
