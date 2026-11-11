//! 视频处理：RIFE 显卡插帧，以及压缩转码 / 无损剪切 / 转 GIF / 提取音频。
//! 任务在后台线程运行，通过 `Job` 暴露进度与结果，CLI 与 GUI 共用。
//!
//! 插帧管线（三段并行，磁盘上同时最多留两三段临时帧）：
//!   ffmpeg 解码 → PNG 流 → 按段落盘 → rife-ncnn-vulkan 逐段插帧 → PNG 流 → ffmpeg 编码（音频直接取自源文件）
//!
//! rife `-n T` 的语义：输出第 i 帧 = 输入时刻 i×F/T（F 为输入帧数，超出末帧的部分复制末帧）。
//! 所以每段多带 m 帧「前瞻」：喂 F+m 帧、要 (F+m)×r 帧、只取前 F×r 帧，段与段之间严格衔接。
//! 倍率 r = p/q：q ≤ 32 时 F、m 取 q 的倍数，时间点完全精确；q 很大（如 29.97→60）时 m=1 近似。

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::encoder::{self, Backend, Codec};
use crate::video::BoxError;

const RIFE_EXE: &str = "rife-ncnn-vulkan.exe";

// ---------- 源信息 ----------

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub width: u32,
    pub height: u32,
    /// 帧率（分子, 分母）
    pub fps: (u64, u64),
    /// 时长（秒），未知为 0
    pub duration: f64,
    pub video_codec: String,
    pub audio_codec: Option<String>,
    /// 源画面按 BT.601 矩阵解码（标清，或文件里明确标注）
    pub bt601: bool,
}

impl MediaInfo {
    pub fn fps_f64(&self) -> f64 {
        self.fps.0 as f64 / self.fps.1 as f64
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a.max(1)
    } else {
        gcd(b, a % b)
    }
}

fn reduce(n: u64, d: u64) -> (u64, u64) {
    let g = gcd(n, d);
    (n / g, d / g)
}

fn parse_ratio(s: &str) -> Option<(u64, u64)> {
    let (n, d) = s.split_once('/')?;
    let (n, d) = (n.trim().parse::<u64>().ok()?, d.trim().parse::<u64>().ok()?);
    (n > 0 && d > 0).then(|| reduce(n, d))
}

fn hidden(mut cmd: Command) -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

fn ffprobe_path(ffmpeg: &Path) -> PathBuf {
    match ffmpeg.parent() {
        Some(p) if !p.as_os_str().is_empty() => ffmpeg.with_file_name("ffprobe.exe"),
        _ => PathBuf::from("ffprobe"),
    }
}

/// 读取视频的分辨率 / 帧率 / 时长 / 编码信息。
pub fn probe(ffmpeg: &Path, src: &Path) -> Result<MediaInfo, BoxError> {
    let out = hidden(Command::new(ffprobe_path(ffmpeg)))
        .args([
            "-v", "error", "-show_entries",
            "stream=codec_type,codec_name,width,height,r_frame_rate,avg_frame_rate,color_space:format=duration",
            "-of", "compact=p=1:nk=0",
        ])
        .arg(src)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("无法运行 ffprobe（应与 ffmpeg 在同一目录）: {e}"))?;
    if !out.status.success() {
        return Err(format!("无法读取视频信息: {}", String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut video: Option<MediaInfo> = None;
    let mut audio_codec = None;
    let mut duration = 0.0;
    for line in text.lines() {
        let mut parts = line.trim().split('|');
        let section = parts.next().unwrap_or("");
        let get = |key: &str| {
            line.trim().split('|').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v.to_string()))
        };
        match section {
            "format" => duration = get("duration").and_then(|d| d.parse::<f64>().ok()).unwrap_or(0.0),
            "stream" => match get("codec_type").as_deref() {
                Some("video") if video.is_none() => {
                    let width = get("width").and_then(|v| v.parse().ok()).unwrap_or(0);
                    let height: u32 = get("height").and_then(|v| v.parse().ok()).unwrap_or(0);
                    let r = get("r_frame_rate").and_then(|v| parse_ratio(&v));
                    let avg = get("avg_frame_rate").and_then(|v| parse_ratio(&v));
                    let fps = match (r, avg) {
                        (Some(r), Some(a)) => {
                            let (rf, af) = (r.0 as f64 / r.1 as f64, a.0 as f64 / a.1 as f64);
                            // 两者接近是恒定帧率；否则是可变帧率，按平均帧率处理
                            if (rf / af - 1.0).abs() < 0.02 {
                                r
                            } else {
                                reduce((af * 1000.0).round().max(1.0) as u64, 1000)
                            }
                        }
                        (Some(r), None) => r,
                        (None, Some(a)) => a,
                        (None, None) => return Err("无法确定源视频的帧率".into()),
                    };
                    let cs = get("color_space").unwrap_or_default();
                    let bt601 = match cs.as_str() {
                        "bt470bg" | "smpte170m" => true,
                        "bt709" => false,
                        _ => height < 720,
                    };
                    video = Some(MediaInfo {
                        width,
                        height,
                        fps,
                        duration: 0.0,
                        video_codec: get("codec_name").unwrap_or_default(),
                        audio_codec: None,
                        bt601,
                    });
                }
                Some("audio") if audio_codec.is_none() => audio_codec = get("codec_name"),
                _ => {}
            },
            _ => {}
        }
    }
    let mut info = video.ok_or("文件里没有视频画面")?;
    if info.width == 0 || info.height == 0 {
        return Err("无法确定源视频的分辨率".into());
    }
    info.duration = duration;
    info.audio_codec = audio_codec;
    Ok(info)
}

// ---------- 任务描述 ----------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FpsTarget {
    /// 目标帧率（如 60、120、59.94）
    Fps(f64),
    /// 源帧率的整数倍
    Times(u32),
}

/// 目标帧率的精确分数表示。
pub fn target_fps(src: (u64, u64), target: FpsTarget) -> (u64, u64) {
    match target {
        FpsTarget::Times(k) => reduce(src.0 * k.max(1) as u64, src.1),
        FpsTarget::Fps(f) => {
            let r = f.round();
            if (f - r).abs() < 1e-6 {
                (r.max(1.0) as u64, 1)
            } else if (f - r * 1000.0 / 1001.0).abs() < 0.01 {
                // 23.976 / 29.97 / 59.94 / 119.88
                (r as u64 * 1000, 1001)
            } else {
                reduce((f * 1000.0).round().max(1.0) as u64, 1000)
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct InterpOpts {
    pub target: FpsTarget,
    /// 模型目录名（位于 rife 程序目录下），如 rife-v4.6
    pub model: String,
    /// 显卡编号；None 让 rife 自己选（多显卡笔记本上常会选到核显）
    pub gpu: Option<u32>,
    pub codec: Codec,
    pub backend: Backend,
    pub quality: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
pub enum AudioFmt {
    Mp3,
    M4a,
    Wav,
}

impl AudioFmt {
    pub fn ext(self) -> &'static str {
        match self {
            AudioFmt::Mp3 => "mp3",
            AudioFmt::M4a => "m4a",
            AudioFmt::Wav => "wav",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Op {
    /// 显卡插帧，提高帧率
    Interp(InterpOpts),
    /// 重新编码：可同时缩小分辨率 / 降低帧率
    Compress { codec: Codec, backend: Backend, quality: u32, height: Option<u32>, fps: Option<u32> },
    /// 无损剪切（流拷贝，起点对齐到关键帧）
    Cut,
    Gif { width: u32, fps: u32 },
    Audio(AudioFmt),
}

pub struct JobSpec {
    pub src: PathBuf,
    pub dst: PathBuf,
    /// 只处理 [start, end) 这一段（秒）；start=0 且 end=None 表示整段
    pub start: f64,
    pub end: Option<f64>,
    pub op: Op,
    pub ffmpeg: PathBuf,
    /// rife-ncnn-vulkan.exe 的路径（仅插帧需要）
    pub rife: Option<PathBuf>,
}

/// 找 rife-ncnn-vulkan：显式路径 > 程序目录\rife > 源码树 tools\rife（开发时）。
pub fn find_rife(explicit: Option<&Path>) -> Result<PathBuf, BoxError> {
    if let Some(p) = explicit {
        let p = if p.is_dir() { p.join(RIFE_EXE) } else { p.to_path_buf() };
        return if p.exists() { Ok(p) } else { Err(format!("指定的 rife 不存在: {}", p.display()).into()) };
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("rife").join(RIFE_EXE);
            if p.exists() {
                return Ok(p);
            }
            let mut up = Some(dir);
            while let Some(d) = up {
                let p = d.join("tools").join("rife").join(RIFE_EXE);
                if p.exists() {
                    return Ok(p);
                }
                up = d.parent();
            }
        }
    }
    Err("找不到 rife-ncnn-vulkan：应位于程序目录的 rife 文件夹（install.cmd 会自动复制）".into())
}

/// rife 程序目录下可用的模型（目录名）。
pub fn rife_models(rife: &Path) -> Vec<String> {
    let Some(dir) = rife.parent() else { return Vec::new() };
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .filter(|e| e.path().join("flownet.param").exists())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v.reverse();
    v
}

/// 解析时间：`秒`、`分:秒`、`时:分:秒`（秒可带小数）。
pub fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total = 0.0;
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() > 3 {
        return None;
    }
    for p in parts {
        let v: f64 = p.trim().parse().ok()?;
        if v < 0.0 {
            return None;
        }
        total = total * 60.0 + v;
    }
    Some(total)
}

fn fmt_fps(f: f64) -> String {
    if (f - f.round()).abs() < 0.005 {
        format!("{}", f.round() as u64)
    } else {
        format!("{f:.2}")
    }
}

/// 默认输出路径：源文件旁边，按处理方式命名，已存在则加序号。
pub fn default_dst(src: &Path, op: &Op, info: Option<&MediaInfo>) -> PathBuf {
    let stem = src.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "video".into());
    let ext = src.extension().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "mp4".into());
    let name = match op {
        Op::Interp(o) => match (info, o.target) {
            (Some(i), t) => {
                let (n, d) = target_fps(i.fps, t);
                format!("{stem}_{}fps.mp4", fmt_fps(n as f64 / d as f64))
            }
            (None, FpsTarget::Times(k)) => format!("{stem}_x{k}.mp4"),
            (None, FpsTarget::Fps(f)) => format!("{stem}_{}fps.mp4", fmt_fps(f)),
        },
        Op::Compress { .. } => format!("{stem}_compressed.mp4"),
        Op::Cut => format!("{stem}_cut.{ext}"),
        Op::Gif { .. } => format!("{stem}.gif"),
        Op::Audio(f) => format!("{stem}.{}", f.ext()),
    };
    let dir = src.parent().map(Path::to_path_buf).unwrap_or_default();
    let first = dir.join(&name);
    if !first.exists() {
        return first;
    }
    let (base, ext) = match name.rsplit_once('.') {
        Some((b, e)) => (b.to_string(), e.to_string()),
        None => (name.clone(), String::new()),
    };
    (2..10000)
        .map(|i| dir.join(format!("{base}_{i}.{ext}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

// ---------- 任务 ----------

pub struct Job {
    cancel: AtomicBool,
    progress: AtomicU32,
    stage: Mutex<String>,
    result: Mutex<Option<Result<(), String>>>,
    pub dst: PathBuf,
    pub started: Instant,
}

impl Job {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    /// 进度 0.0 ~ 1.0
    pub fn progress(&self) -> f32 {
        f32::from_bits(self.progress.load(Ordering::Relaxed))
    }
    pub fn stage(&self) -> String {
        self.stage.lock().unwrap().clone()
    }
    /// 结束后返回结果；进行中返回 None。
    pub fn result(&self) -> Option<Result<(), String>> {
        self.result.lock().unwrap().clone()
    }
    fn set_progress(&self, p: f64) {
        self.progress.store((p.clamp(0.0, 1.0) as f32).to_bits(), Ordering::Relaxed);
    }
    fn set_stage(&self, s: impl Into<String>) {
        *self.stage.lock().unwrap() = s.into();
    }
}

/// 启动处理任务（立即返回）。
pub fn start(spec: JobSpec) -> Arc<Job> {
    let job = Arc::new(Job {
        cancel: AtomicBool::new(false),
        progress: AtomicU32::new(0),
        stage: Mutex::new("准备中".into()),
        result: Mutex::new(None),
        dst: spec.dst.clone(),
        started: Instant::now(),
    });
    let j = job.clone();
    std::thread::spawn(move || {
        let mut r = run(&spec, &j).map_err(|e| e.to_string());
        if j.cancelled() {
            r = Err("已取消".into());
        }
        if r.is_err() && spec.dst != spec.src {
            // 不留半截文件
            let _ = std::fs::remove_file(&spec.dst);
        }
        *j.result.lock().unwrap() = Some(r);
    });
    job
}

fn run(spec: &JobSpec, job: &Arc<Job>) -> Result<(), BoxError> {
    if spec.src == spec.dst {
        return Err("输出文件不能与源文件相同".into());
    }
    job.set_stage("读取视频信息");
    let info = probe(&spec.ffmpeg, &spec.src)?;
    let start = spec.start.max(0.0);
    if let Some(e) = spec.end {
        if e <= start {
            return Err("片段终点必须大于起点".into());
        }
    }
    let span = (spec.end.unwrap_or(info.duration) - start).max(0.001);
    if let Some(parent) = spec.dst.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    match &spec.op {
        Op::Interp(o) => run_interp(spec, job, &info, o, start, span),
        _ => run_simple(spec, job, &info, start, span),
    }
}

fn seek_args(start: f64, end: Option<f64>) -> Vec<String> {
    let mut a = Vec::new();
    if start > 0.0 {
        a.push("-ss".to_string());
        a.push(format!("{start:.3}"));
    }
    if let Some(e) = end {
        a.push("-to".to_string());
        a.push(format!("{e:.3}"));
    }
    a
}

fn is_mp4(dst: &Path) -> bool {
    let ext = dst.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    matches!(ext.as_str(), "mp4" | "mov" | "m4v")
}

/// 音轨能直接放进目标容器就原样拷贝，否则转 AAC。
fn audio_args(info: &MediaInfo, dst: &Path) -> Vec<&'static str> {
    let copy_ok = !is_mp4(dst)
        || matches!(info.audio_codec.as_deref(), Some("aac" | "mp3" | "ac3" | "eac3" | "alac" | "opus" | "flac"));
    if info.audio_codec.is_none() {
        Vec::new()
    } else if copy_ok {
        vec!["-c:a", "copy"]
    } else {
        vec!["-c:a", "aac", "-b:a", "256k"]
    }
}

fn pad_even(info: &MediaInfo) -> Option<&'static str> {
    (info.width % 2 == 1 || info.height % 2 == 1).then_some("pad=ceil(iw/2)*2:ceil(ih/2)*2")
}

/// 逐行读取（容忍非 UTF-8 输出），保证管道被读空。
fn for_each_line(r: impl Read, mut f: impl FnMut(&str)) {
    let mut r = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => f(String::from_utf8_lossy(&buf).trim_end()),
        }
    }
}

/// 后台读走子进程的 stderr，保留最后 30 行用于报错。
fn drain_stderr(child: &mut Child) -> Arc<Mutex<Vec<String>>> {
    let tail = Arc::new(Mutex::new(Vec::new()));
    if let Some(err) = child.stderr.take() {
        let t = tail.clone();
        std::thread::spawn(move || {
            for_each_line(err, |line| {
                let mut v = t.lock().unwrap();
                if v.len() >= 30 {
                    v.remove(0);
                }
                v.push(line.to_string());
            });
        });
    }
    tail
}

fn tail_text(tail: &Arc<Mutex<Vec<String>>>) -> String {
    tail.lock().unwrap().join("\n")
}

fn wait_or_cancel(child: &mut Child, job: &Job) -> std::io::Result<ExitStatus> {
    loop {
        if let Some(s) = child.try_wait()? {
            return Ok(s);
        }
        if job.cancelled() {
            let _ = child.kill();
            return child.wait();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 离开作用域时确保子进程已结束。
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 压缩转码 / 剪切 / GIF / 提取音频：一次 ffmpeg 调用。
fn run_simple(spec: &JobSpec, job: &Arc<Job>, info: &MediaInfo, start: f64, span: f64) -> Result<(), BoxError> {
    let mut cmd = hidden(Command::new(&spec.ffmpeg));
    cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-nostdin", "-progress", "pipe:1", "-nostats"]);
    cmd.args(seek_args(start, spec.end)).arg("-i").arg(&spec.src);
    match &spec.op {
        Op::Compress { codec, backend, quality, height, fps } => {
            job.set_stage("探测编码器");
            let (backend, enc) = encoder::pick_backend(&spec.ffmpeg, *codec, *backend)?;
            let mut vf: Vec<String> = Vec::new();
            if let Some(f) = fps.filter(|f| (*f as f64) < info.fps_f64() - 0.01) {
                vf.push(format!("fps={f}"));
            }
            let matrix = if info.bt601 { "bt601" } else { "bt709" };
            let size = match height.filter(|h| *h < info.height) {
                Some(h) => format!("-2:{h}:flags=lanczos+accurate_rnd+full_chroma_int:"),
                None => {
                    vf.extend(pad_even(info).map(String::from));
                    "flags=accurate_rnd+full_chroma_int:".to_string()
                }
            };
            vf.push(format!(
                "scale={size}in_color_matrix={matrix}:out_color_matrix=bt709:out_range=tv,format=yuv420p"
            ));
            cmd.args(["-map", "0:v:0", "-map", "0:a?", "-vf", &vf.join(",")]);
            cmd.args(encoder::video_args(*codec, backend, enc, *quality, false));
            cmd.args(audio_args(info, &spec.dst));
            if is_mp4(&spec.dst) {
                cmd.args(["-movflags", "+faststart"]);
            }
        }
        Op::Cut => {
            cmd.args(["-map", "0:v?", "-map", "0:a?", "-c", "copy", "-avoid_negative_ts", "make_zero"]);
        }
        Op::Gif { width, fps } => {
            let w = (*width).clamp(16, info.width.max(16));
            let vf = format!(
                "fps={fps},scale={w}:-1:flags=lanczos,split[a][b];[a]palettegen=stats_mode=diff[p];\
                 [b][p]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle"
            );
            cmd.args(["-vf", &vf, "-loop", "0"]);
        }
        Op::Audio(fmt) => {
            if info.audio_codec.is_none() {
                return Err("该视频没有音轨".into());
            }
            cmd.args(["-vn", "-map", "0:a:0"]);
            match fmt {
                AudioFmt::Mp3 => cmd.args(["-c:a", "libmp3lame", "-q:a", "2"]),
                AudioFmt::M4a if info.audio_codec.as_deref() == Some("aac") => cmd.args(["-c:a", "copy"]),
                AudioFmt::M4a => cmd.args(["-c:a", "aac", "-b:a", "256k"]),
                AudioFmt::Wav => cmd.args(["-c:a", "pcm_s16le"]),
            };
        }
        Op::Interp(_) => unreachable!(),
    }
    cmd.arg(&spec.dst).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    job.set_stage("处理中");
    let mut child = Guard(cmd.spawn()?);
    let tail = drain_stderr(&mut child.0);
    if let Some(out) = child.0.stdout.take() {
        let j = job.clone();
        std::thread::spawn(move || {
            for_each_line(out, |line| {
                if let Some(us) = line.strip_prefix("out_time_us=").and_then(|v| v.trim().parse::<f64>().ok()) {
                    j.set_progress(us / 1e6 / span);
                }
            });
        });
    }
    let status = wait_or_cancel(&mut child.0, job)?;
    if job.cancelled() {
        return Err("已取消".into());
    }
    if !status.success() {
        return Err(format!("ffmpeg 处理失败\n{}", tail_text(&tail)).into());
    }
    job.set_progress(1.0);
    Ok(())
}

// ---------- 插帧 ----------

/// 从 image2pipe 流里读出一张完整的 PNG；流结束返回 false。
fn read_png(r: &mut impl Read, buf: &mut Vec<u8>) -> std::io::Result<bool> {
    const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    buf.clear();
    let mut sig = [0u8; 8];
    let mut got = 0;
    while got < 8 {
        let n = r.read(&mut sig[got..])?;
        if n == 0 {
            if got == 0 {
                return Ok(false);
            }
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        got += n;
    }
    if sig != SIG {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "解码输出不是 PNG"));
    }
    buf.extend_from_slice(&sig);
    loop {
        let mut hdr = [0u8; 8];
        r.read_exact(&mut hdr)?;
        let len = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        buf.extend_from_slice(&hdr);
        let at = buf.len();
        buf.resize(at + len + 4, 0); // 数据 + CRC
        r.read_exact(&mut buf[at..])?;
        if &hdr[4..8] == b"IEND" {
            return Ok(true);
        }
    }
}

fn list_sorted(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|it| it.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// 一段待插帧的源帧：目录里是 00000001.png 起的 `count` 帧，第一帧是全片第 `first` 帧。
struct InChunk {
    dir: PathBuf,
    count: u64,
    first: u64,
    last: bool,
}

/// 一段插好的帧：按顺序写给编码器，然后删掉这些临时目录。
struct OutChunk {
    files: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
}

fn run_interp(
    spec: &JobSpec,
    job: &Arc<Job>,
    info: &MediaInfo,
    o: &InterpOpts,
    start: f64,
    span: f64,
) -> Result<(), BoxError> {
    let rife = spec.rife.clone().ok_or("找不到 rife-ncnn-vulkan（应位于程序目录的 rife 文件夹）")?;
    let rife_dir = rife.parent().map(Path::to_path_buf).unwrap_or_default();
    let model = rife_dir.join(&o.model);
    if !model.join("flownet.param").exists() {
        return Err(format!("找不到插帧模型: {}", model.display()).into());
    }
    let (sn, sd) = info.fps;
    let (tn, td) = target_fps(info.fps, o.target);
    // 倍率 r = 目标帧率 / 源帧率 = p/q
    let (p, q) = reduce(tn * sd, td * sn);
    if p <= q {
        return Err(format!(
            "目标帧率（{}）必须高于源帧率（{}）",
            fmt_fps(tn as f64 / td as f64),
            fmt_fps(info.fps_f64())
        )
        .into());
    }
    if p > q * 16 {
        return Err("最多支持 16 倍插帧".into());
    }
    let exact = q <= 32;
    let m = if exact { q } else { 1 };
    // 每段的输出帧数按像素量控制，1080p 约 480 帧（临时 PNG 约 1~2 GB）
    let px = (info.width as f64 * info.height as f64).max(1.0);
    let budget = (480.0 * 1920.0 * 1080.0 / px).clamp(48.0, 960.0);
    let mut f = ((budget * q as f64 / p as f64) as u64).max(2);
    if exact {
        f = (f / q).max(1) * q;
    }
    let scaled = move |x: u64| (x * p + q / 2) / q; // round(x × r)
    let total_out = (span * tn as f64 / td as f64).max(1.0);

    job.set_stage("探测编码器");
    let (backend, enc) = encoder::pick_backend(&spec.ffmpeg, o.codec, o.backend)?;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("screenrec_interp_{}_{stamp}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let result = (|| -> Result<(), BoxError> {
        // ---- 解码：源视频 → PNG 流 ----
        let matrix = if info.bt601 { "bt601" } else { "bt709" };
        let mut dec = hidden(Command::new(&spec.ffmpeg));
        dec.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
            .args(seek_args(start, spec.end))
            .arg("-i")
            .arg(&spec.src)
            .args(["-map", "0:v:0", "-vf"])
            .arg(format!(
                "fps={sn}/{sd},scale=in_color_matrix={matrix}:flags=accurate_rnd+full_chroma_int,format=rgb24"
            ))
            .args(["-f", "image2pipe", "-c:v", "png", "-compression_level", "1", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut dec = Guard(dec.spawn()?);
        let dec_tail = drain_stderr(&mut dec.0);
        let dec_out = dec.0.stdout.take().ok_or("无法获取解码输出")?;

        // ---- 编码：PNG 流 + 源音频 → 目标文件 ----
        let mut vf: Vec<&str> = pad_even(info).into_iter().collect();
        vf.push("scale=out_color_matrix=bt709:out_range=tv:flags=accurate_rnd+full_chroma_int,format=yuv420p,setparams=colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv");
        let mut encode = hidden(Command::new(&spec.ffmpeg));
        encode
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "image2pipe", "-c:v", "png", "-framerate"])
            .arg(format!("{tn}/{td}"))
            .args(["-i", "pipe:0"])
            .args(seek_args(start, spec.end))
            .arg("-i")
            .arg(&spec.src)
            .args(["-map", "0:v:0", "-map", "1:a?", "-vf", &vf.join(",")])
            .args(encoder::video_args(o.codec, backend, enc, o.quality, false))
            .args(audio_args(info, &spec.dst));
        if is_mp4(&spec.dst) {
            encode.args(["-movflags", "+faststart"]);
        }
        encode.arg(&spec.dst).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut encode = Guard(encode.spawn()?);
        let enc_tail = drain_stderr(&mut encode.0);
        let enc_in = encode.0.stdin.take().ok_or("无法获取编码输入")?;

        // ---- 读线程：PNG 流按段落盘 ----
        let (tx_in, rx_in) = sync_channel::<InChunk>(1);
        let reader = {
            let tmp = tmp.clone();
            std::thread::spawn(move || -> Result<u64, String> {
                let io = |e: std::io::Error| format!("写临时文件失败: {e}");
                let mut r = BufReader::with_capacity(1 << 20, dec_out);
                let mut buf = Vec::new();
                let (mut k, mut first, mut count) = (0u64, 0u64, 0u64);
                let mut dir = tmp.join(format!("in{k}"));
                std::fs::create_dir_all(&dir).map_err(io)?;
                while read_png(&mut r, &mut buf).map_err(|e| format!("读取解码帧失败: {e}"))? {
                    count += 1;
                    std::fs::write(dir.join(format!("{count:08}.png")), &buf).map_err(io)?;
                    if count == f + m {
                        // 末尾 m 帧是「前瞻」，同时也是下一段的开头
                        let next = tmp.join(format!("in{}", k + 1));
                        std::fs::create_dir_all(&next).map_err(io)?;
                        for j in 1..=m {
                            let from = dir.join(format!("{:08}.png", f + j));
                            let to = next.join(format!("{j:08}.png"));
                            if std::fs::hard_link(&from, &to).is_err() {
                                std::fs::copy(&from, &to).map_err(io)?;
                            }
                        }
                        if tx_in.send(InChunk { dir, count, first, last: false }).is_err() {
                            return Ok(first);
                        }
                        dir = next;
                        k += 1;
                        first += f;
                        count = m;
                    }
                }
                if count > 0 {
                    let _ = tx_in.send(InChunk { dir, count, first, last: true });
                }
                Ok(first + count)
            })
        };

        // ---- 写线程：插好的帧按顺序喂给编码器 ----
        let (tx_out, rx_out) = sync_channel::<OutChunk>(1);
        let writer = std::thread::spawn(move || -> Result<(), String> {
            let mut stdin = enc_in;
            for c in rx_out {
                for file in &c.files {
                    let data = std::fs::read(file).map_err(|e| format!("读取插帧结果失败: {e}"))?;
                    stdin.write_all(&data).map_err(|e| format!("写入编码器失败: {e}"))?;
                }
                for d in &c.dirs {
                    let _ = std::fs::remove_dir_all(d);
                }
            }
            Ok(())
        });

        // ---- 主循环：逐段调用 rife ----
        let main = (|| -> Result<(), BoxError> {
            let mut done_out = 0u64;
            let mut k = 0u64;
            while let Ok(chunk) = rx_in.recv() {
                if job.cancelled() {
                    break;
                }
                k += 1;
                let (want, keep) = if chunk.last {
                    let t = (scaled(chunk.first + chunk.count) - scaled(chunk.first)).max(1);
                    (t, t)
                } else {
                    (scaled(chunk.count), scaled(chunk.first + f) - scaled(chunk.first))
                };
                let out_dir = tmp.join(format!("out{k}"));
                let files = if chunk.count < 2 {
                    // 只剩一帧，没有可插的，直接重复
                    vec![chunk.dir.join("00000001.png"); keep as usize]
                } else {
                    std::fs::create_dir_all(&out_dir)?;
                    job.set_stage(format!("插帧中（第 {k} 段）"));
                    let mut cmd = hidden(Command::new(&rife));
                    cmd.arg("-i").arg(&chunk.dir).arg("-o").arg(&out_dir);
                    cmd.args(["-n", &want.to_string(), "-f", "%08d.png", "-m"]).arg(&model);
                    if let Some(g) = o.gpu {
                        cmd.args(["-g", &g.to_string()]);
                    }
                    cmd.current_dir(&rife_dir).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
                    let mut child =
                        Guard(cmd.spawn().map_err(|e| format!("无法启动 rife-ncnn-vulkan: {e}"))?);
                    let tail = drain_stderr(&mut child.0);
                    let mut tick = Instant::now();
                    while child.0.try_wait()?.is_none() && !job.cancelled() {
                        if tick.elapsed() > Duration::from_millis(500) {
                            tick = Instant::now();
                            let n = (list_sorted(&out_dir).len() as u64).min(keep);
                            job.set_progress((done_out + n) as f64 / total_out);
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    if job.cancelled() {
                        break;
                    }
                    let mut files = list_sorted(&out_dir);
                    if (files.len() as u64) < keep {
                        return Err(format!(
                            "插帧失败：第 {k} 段只生成了 {} / {keep} 帧\n{}",
                            files.len(),
                            tail_text(&tail)
                        )
                        .into());
                    }
                    files.truncate(keep as usize);
                    files
                };
                done_out += keep;
                job.set_progress(done_out as f64 / total_out);
                if tx_out.send(OutChunk { files, dirs: vec![chunk.dir, out_dir] }).is_err() {
                    break; // 写线程已出错退出
                }
            }
            Ok(())
        })();

        // ---- 收尾 ----
        let aborted = main.is_err() || job.cancelled();
        if aborted {
            let _ = dec.0.kill();
            let _ = encode.0.kill();
        }
        drop(rx_in);
        drop(tx_out);
        let frames = reader.join().unwrap_or_else(|_| Err("读线程崩溃".into()));
        let wrote = writer.join().unwrap_or_else(|_| Err("写线程崩溃".into()));
        main?;
        if job.cancelled() {
            return Err("已取消".into());
        }
        job.set_stage("正在写入文件…");
        let dec_status = dec.0.wait()?;
        let enc_status = wait_or_cancel(&mut encode.0, job)?;
        if job.cancelled() {
            return Err("已取消".into());
        }
        let frames = frames?;
        if frames == 0 || !dec_status.success() {
            return Err(format!("解码源视频失败\n{}", tail_text(&dec_tail)).into());
        }
        if let Err(e) = wrote {
            return Err(format!("{e}\n{}", tail_text(&enc_tail)).into());
        }
        if !enc_status.success() {
            return Err(format!("编码失败\n{}", tail_text(&enc_tail)).into());
        }
        job.set_progress(1.0);
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}
