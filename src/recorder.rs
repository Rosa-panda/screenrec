//! 录制会话：把抓屏、音频配速、编码器串起来，在后台线程里跑，
//! 通过 `SessionState` 对外暴露进度，CLI 与 GUI 共用。
//!
//! 三种引擎：
//! - Standard：画面读回 CPU → ffmpeg（swscale 转 BT.709 yuv420p）→ 硬件编码
//! - Game：同上，但用更轻的编码预设，并降低本工具与 ffmpeg 的 CPU/GPU 优先级
//! - ZeroCopy：画面留在 GPU，Media Foundation 硬件编码；音频本进程混音

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D11::{ID3D11DeviceContext, ID3D11Texture2D};

use crate::audio::{self, AudioInput};
use crate::clock::Clock;
use crate::encoder::{self, Backend, Codec};
use crate::mf::{MfAudio, MfConfig, MfWriter};
use crate::video::{self, BoxError, FrameStore, SendD3D, Target};

/// 录制引擎
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
pub enum Engine {
    /// 标准：ffmpeg 编码，画质与兼容性最好
    #[default]
    Standard,
    /// 游戏：ffmpeg 编码，轻量预设，并让出 CPU/GPU 优先级
    Game,
    /// 零拷贝：Media Foundation 硬件编码，画面不经过 CPU；仅 mp4 + H.264/HEVC，码率模式
    ZeroCopy,
}

pub struct RecordConfig {
    pub output: PathBuf,
    pub target: Target,
    pub cursor: bool,
    pub fps: u32,
    pub codec: Codec,
    pub backend: Backend,
    pub quality: u32,
    pub audio_bitrate: u32,
    pub separate_tracks: bool,
    pub audio: Vec<AudioInput>,
    pub duration: Option<f64>,
    pub ffmpeg: PathBuf,
    pub engine: Engine,
    /// ffmpeg 不显示控制台窗口（GUI 用）
    pub hide_console: bool,
    /// 把 ffmpeg 的 stderr 转发到本进程 stderr（CLI 用）
    pub echo_ffmpeg: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Starting,
    Recording,
    Finishing,
    Done,
    Failed,
}

pub struct SessionState {
    pub stop: AtomicBool,
    pub phase: Mutex<Phase>,
    /// 录制时钟（暂停时停走），视频/音频配速共用
    pub clock: Arc<Clock>,
    pub sent: AtomicU64,
    pub captured: AtomicU64,
    pub size: AtomicU64,
    pub encoder_name: Mutex<String>,
    pub frame_size: Mutex<(u32, u32)>,
    pub error: Mutex<Option<String>>,
    /// ffmpeg stderr 或零拷贝引擎的诊断信息（最近 50 行）
    pub ffmpeg_log: Mutex<Vec<String>>,
    pub exit_reason: Mutex<String>,
    pub output: PathBuf,
}

impl SessionState {
    pub fn phase(&self) -> Phase {
        *self.phase.lock().unwrap()
    }
    fn set_phase(&self, p: Phase) {
        *self.phase.lock().unwrap() = p;
    }
    /// 已录制时长（秒，不含暂停）
    pub fn elapsed(&self) -> f64 {
        self.clock.elapsed()
    }
    pub fn is_paused(&self) -> bool {
        self.clock.is_paused()
    }
    /// 暂停 / 继续（只在录制中有效）
    pub fn toggle_pause(&self) {
        if self.phase() == Phase::Recording {
            self.clock.set_paused(!self.clock.is_paused());
        }
    }
    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
    fn log(&self, line: String) {
        let mut log = self.ffmpeg_log.lock().unwrap();
        if log.len() > 50 {
            log.remove(0);
        }
        log.push(line);
    }
}

pub struct Session {
    pub state: Arc<SessionState>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    /// 启动录制（立即返回，所有工作在后台线程）。
    pub fn start(cfg: RecordConfig) -> Session {
        let state = Arc::new(SessionState {
            stop: AtomicBool::new(false),
            phase: Mutex::new(Phase::Starting),
            clock: Arc::new(Clock::default()),
            sent: AtomicU64::new(0),
            captured: AtomicU64::new(0),
            size: AtomicU64::new(0),
            encoder_name: Mutex::new(String::new()),
            frame_size: Mutex::new((0, 0)),
            error: Mutex::new(None),
            ffmpeg_log: Mutex::new(Vec::new()),
            exit_reason: Mutex::new(String::new()),
            output: cfg.output.clone(),
        });
        let st = state.clone();
        let thread = std::thread::spawn(move || {
            if let Err(e) = run_session(cfg, &st) {
                let mut msg = e.to_string();
                let log = st.ffmpeg_log.lock().unwrap();
                if !log.is_empty() {
                    msg.push_str("\n日志: ");
                    msg.push_str(&log.join("\n"));
                }
                *st.error.lock().unwrap() = Some(msg);
                st.set_phase(Phase::Failed);
            } else {
                st.set_phase(Phase::Done);
            }
        });
        Session { state, thread: Some(thread) }
    }

    pub fn stop(&self) {
        self.state.request_stop();
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.state.phase(), Phase::Done | Phase::Failed)
    }

    /// 阻塞直到会话结束。
    pub fn wait(mut self) -> Result<(), String> {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        match self.state.error() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

struct AudioPipe {
    listener: TcpListener,
    port: u16,
}

fn new_audio_pipe() -> Result<AudioPipe, BoxError> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    Ok(AudioPipe { listener, port })
}

fn accept_with_timeout(listener: TcpListener, stop: &AtomicBool) -> Option<TcpStream> {
    listener.set_nonblocking(true).ok()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match listener.accept() {
            Ok((s, _)) => {
                s.set_nonblocking(false).ok()?;
                let _ = s.set_nodelay(true);
                return Some(s);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Relaxed) || Instant::now() > deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

/// 第 k 帧（k 从 0 起）应在录制时刻 k/fps 发出；此刻应已发出的帧数 = floor(elapsed*fps)+1
fn frames_due(clock: &Clock, fps: u32) -> u64 {
    (clock.elapsed() * fps as f64) as u64 + 1
}

fn sleep_until_frame(clock: &Clock, fps: u32, n: u64) {
    if clock.is_paused() {
        std::thread::sleep(Duration::from_millis(10));
        return;
    }
    let wait = n as f64 / fps as f64 - clock.elapsed();
    if wait > 0.0 {
        std::thread::sleep(Duration::from_secs_f64(wait));
    }
}

fn video_pacer(
    store: Arc<FrameStore>,
    fps: u32,
    stop: Arc<AtomicBool>,
    sent: Arc<SessionState>,
    mut stdin: ChildStdin,
) {
    let clock = sent.clock.clone();
    let mut local: Vec<u8> = Vec::new();
    let mut n: u64 = 0;
    loop {
        let stopping = stop.load(Ordering::Relaxed);
        let target = frames_due(&clock, fps);
        while n < target {
            {
                let slot = store.slot.lock().unwrap();
                local.clear();
                local.extend_from_slice(&slot.data);
            }
            if stdin.write_all(&local).is_err() {
                stop.store(true, Ordering::Relaxed);
                return;
            }
            n += 1;
            sent.sent.store(n, Ordering::Relaxed);
        }
        if stopping {
            break;
        }
        sleep_until_frame(&clock, fps, n);
    }
    let _ = stdin.flush();
    // drop(stdin) => ffmpeg 收到 EOF
}

const POOL: usize = 8;

/// 零拷贝配速：把最新 GPU 帧拷进帧池纹理，直接交给 Media Foundation。
#[allow(clippy::too_many_arguments)]
fn gpu_pacer(
    context: SendD3D<ID3D11DeviceContext>,
    latest: SendD3D<ID3D11Texture2D>,
    pool: SendD3D<Vec<ID3D11Texture2D>>,
    writer: Arc<Mutex<MfWriter>>,
    fps: u32,
    stop: Arc<AtomicBool>,
    st: Arc<SessionState>,
) -> Result<(), String> {
    let clock = st.clock.clone();
    let mut n: u64 = 0;
    loop {
        let stopping = stop.load(Ordering::Relaxed);
        let target = frames_due(&clock, fps);
        while n < target {
            let tex = &pool.0[(n % POOL as u64) as usize];
            unsafe {
                context.0.CopyResource(tex, &latest.0);
            }
            if let Err(e) = writer.lock().unwrap().write_video(tex, n) {
                stop.store(true, Ordering::Relaxed);
                return Err(format!("视频写入失败: {e}"));
            }
            n += 1;
            st.sent.store(n, Ordering::Relaxed);
        }
        if stopping {
            break;
        }
        sleep_until_frame(&clock, fps, n);
    }
    Ok(())
}

fn spawn_ffmpeg(
    cfg: &RecordConfig,
    enc_name: &str,
    size: (u32, u32),
    ports: &[u16],
    output: &Path,
) -> Result<Child, BoxError> {
    let (w, h) = size;
    let game = cfg.engine == Engine::Game;
    let backend = cfg_backend(cfg, enc_name);
    let mut cmd = Command::new(&cfg.ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
    // 视频输入：stdin 原始 BGRA
    cmd.args([
        "-thread_queue_size", "64",
        "-f", "rawvideo", "-pix_fmt", "bgra",
        "-video_size", &format!("{w}x{h}"),
        "-framerate", &cfg.fps.to_string(),
        "-i", "pipe:0",
    ]);
    // 音频输入：本地 TCP，f32le 交错
    for (src, port) in cfg.audio.iter().zip(ports) {
        cmd.args([
            "-thread_queue_size", "4096",
            "-f", "f32le",
            "-ar", &src.rate.to_string(),
            "-ac", &src.channels.to_string(),
            "-i", &format!("tcp://127.0.0.1:{port}"),
        ]);
    }
    // 视频滤镜：奇数尺寸补齐 + BT.709 正确转 yuv420p
    let mut vf: Vec<&str> = Vec::new();
    if w % 2 == 1 || h % 2 == 1 {
        vf.push("pad=ceil(iw/2)*2:ceil(ih/2)*2");
    }
    vf.push("scale=out_color_matrix=bt709:out_range=tv:flags=accurate_rnd+full_chroma_int,format=yuv420p");
    let vf = vf.join(",");
    if !vf.is_empty() {
        cmd.args(["-vf", &vf]);
    }
    cmd.args(["-map", "0:v"]);

    // 音频映射
    let n = cfg.audio.len();
    if n == 0 {
        cmd.arg("-an");
    } else if n == 1 {
        cmd.args(["-map", "1:a", "-filter:a", &format!("volume={}", cfg.audio[0].volume)]);
        cmd.args(["-metadata:s:a:0", &format!("title={}", cfg.audio[0].label)]);
    } else if cfg.separate_tracks {
        for (i, src) in cfg.audio.iter().enumerate() {
            cmd.args(["-map", &format!("{}:a", i + 1)]);
            cmd.args([&format!("-filter:a:{i}"), &format!("volume={}", src.volume)]);
            cmd.args([&format!("-metadata:s:a:{i}"), &format!("title={}", src.label)]);
        }
    } else {
        let mut graph = String::new();
        let mut labels = String::new();
        for (i, src) in cfg.audio.iter().enumerate() {
            graph.push_str(&format!("[{}:a]volume={}[a{i}];", i + 1, src.volume));
            labels.push_str(&format!("[a{i}]"));
        }
        graph.push_str(&format!("{labels}amix=inputs={n}:duration=longest:normalize=0[aout]"));
        cmd.args(["-filter_complex", &graph, "-map", "[aout]"]);
    }
    if n > 0 {
        cmd.args(["-c:a", "aac", "-b:a", &format!("{}k", cfg.audio_bitrate), "-ar", "48000"]);
    }

    cmd.args(encoder::video_args(cfg.codec, backend, enc_name, cfg.quality, game));
    let ext = output.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if matches!(ext.as_str(), "mp4" | "mov" | "m4v") {
        cmd.args(["-movflags", "+faststart"]);
    }
    cmd.arg(output);

    cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
    {
        use std::os::windows::process::CommandExt;
        // 新进程组：Ctrl+C 只到我们这里，由我们优雅收尾，ffmpeg 不会被直接打断
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        let mut flags = CREATE_NEW_PROCESS_GROUP;
        if cfg.hide_console {
            flags |= CREATE_NO_WINDOW;
        }
        if game {
            // 游戏模式：ffmpeg 让出 CPU，宁可录制掉帧也不拖慢游戏
            flags |= BELOW_NORMAL_PRIORITY_CLASS;
        }
        cmd.creation_flags(flags);
    }
    Ok(cmd.spawn()?)
}

/// `pick_backend` 返回的是实际后端；这里只是把 Auto 解析结果带回 video_args。
fn cfg_backend(cfg: &RecordConfig, enc_name: &str) -> Backend {
    if cfg.backend != Backend::Auto {
        return cfg.backend;
    }
    if enc_name.ends_with("_nvenc") {
        Backend::Nvenc
    } else if enc_name.ends_with("_qsv") {
        Backend::Qsv
    } else if enc_name.ends_with("_amf") {
        Backend::Amf
    } else {
        Backend::Cpu
    }
}

/// 把进程的 GPU 调度优先级降到 BELOW_NORMAL，显卡忙时游戏优先。
fn lower_gpu_priority(process: HANDLE) {
    use windows::core::{s, w};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    unsafe {
        let Ok(lib) = LoadLibraryW(w!("gdi32.dll")) else { return };
        let Some(f) = GetProcAddress(lib, s!("D3DKMTSetProcessSchedulingPriorityClass")) else { return };
        // NTSTATUS D3DKMTSetProcessSchedulingPriorityClass(HANDLE, D3DKMT_SCHEDULINGPRIORITYCLASS)
        let f: unsafe extern "system" fn(HANDLE, i32) -> i32 = std::mem::transmute(f);
        const BELOW_NORMAL: i32 = 1;
        f(process, BELOW_NORMAL);
    }
}

fn codec_label(c: Codec) -> &'static str {
    match c {
        Codec::H264 => "H.264",
        Codec::Hevc => "HEVC",
        Codec::Av1 => "AV1",
    }
}

fn run_session(cfg: RecordConfig, st: &Arc<SessionState>) -> Result<(), BoxError> {
    if let Some(parent) = cfg.output.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let zero = cfg.engine == Engine::ZeroCopy;

    // ---- 编码器 ----
    let mut enc_name = "";
    if zero {
        let ext = cfg.output.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        if ext != "mp4" {
            return Err("零拷贝引擎只支持输出 .mp4".into());
        }
        if cfg.codec == Codec::Av1 {
            return Err("零拷贝引擎暂不支持 AV1，请选 H.264 或 HEVC".into());
        }
        *st.encoder_name.lock().unwrap() = format!("MF {}（零拷贝）", codec_label(cfg.codec));
    } else {
        let (_backend, name) = encoder::pick_backend(&cfg.ffmpeg, cfg.codec, cfg.backend)?;
        *st.encoder_name.lock().unwrap() = name.to_string();
        enc_name = name;
    }

    // ---- 视频抓取 ----
    let store = FrameStore::new(zero);
    let control = video::start(cfg.target.clone(), cfg.cursor, cfg.fps, store.clone())?;
    let (w, h) = match store.wait_first_frame(Duration::from_secs(5)) {
        Some(s) => s,
        None => {
            store.stop.store(true, Ordering::Relaxed);
            let _ = control.stop();
            return Err("5 秒内没有抓到画面（窗口是否最小化？）".into());
        }
    };
    *st.frame_size.lock().unwrap() = (w, h);

    if cfg.engine != Engine::Standard {
        lower_gpu_priority(unsafe { windows::Win32::System::Threading::GetCurrentProcess() });
    }

    let result = if zero {
        run_zero_copy(&cfg, st, &store, w, h)
    } else {
        run_ffmpeg(&cfg, st, &store, enc_name, w, h)
    };
    store.stop.store(true, Ordering::Relaxed);
    let _ = control.stop();
    result
}

/// 主循环：等待停止条件，顺带刷新进度；返回退出原因。
fn main_loop(
    cfg: &RecordConfig,
    st: &Arc<SessionState>,
    store: &Arc<FrameStore>,
    stop: &AtomicBool,
    mut extra: impl FnMut() -> Option<&'static str>,
) -> &'static str {
    let mut exit_reason = "手动停止";
    loop {
        if st.stop.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed) {
            break;
        }
        if let Some(d) = cfg.duration {
            if st.elapsed() >= d {
                exit_reason = "到达设定时长";
                break;
            }
        }
        if store.closed.load(Ordering::Relaxed) {
            exit_reason = "录制目标已关闭";
            break;
        }
        if let Some(r) = extra() {
            exit_reason = r;
            break;
        }
        st.captured.store(store.captured.load(Ordering::Relaxed), Ordering::Relaxed);
        if let Ok(m) = std::fs::metadata(&cfg.output) {
            st.size.store(m.len(), Ordering::Relaxed);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    *st.exit_reason.lock().unwrap() = exit_reason.to_string();
    st.set_phase(Phase::Finishing);
    exit_reason
}

fn run_ffmpeg(
    cfg: &RecordConfig,
    st: &Arc<SessionState>,
    store: &Arc<FrameStore>,
    enc_name: &str,
    w: u32,
    h: u32,
) -> Result<(), BoxError> {
    // ---- 音频通道 ----
    let pipes: Vec<AudioPipe> = cfg.audio.iter().map(|_| new_audio_pipe()).collect::<Result<_, _>>()?;
    let ports: Vec<u16> = pipes.iter().map(|p| p.port).collect();

    // ---- ffmpeg ----
    let mut child = spawn_ffmpeg(cfg, enc_name, (w, h), &ports, &cfg.output)?;
    if cfg.engine == Engine::Game {
        lower_gpu_priority(HANDLE(child.as_raw_handle()));
    }
    let stdin = child.stdin.take().ok_or("无法获取 ffmpeg stdin")?;
    if let Some(stderr) = child.stderr.take() {
        let st2 = st.clone();
        let echo = cfg.echo_ffmpeg;
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if echo {
                    eprintln!("[ffmpeg] {line}");
                }
                st2.log(line);
            }
        });
    }

    // 统一时间起点：此后视频/音频都按同一个录制时钟配速
    for s in &cfg.audio {
        s.ring.lock().unwrap().clear();
    }
    st.clock.start();
    st.set_phase(Phase::Recording);

    let stop = Arc::new(AtomicBool::new(false));
    let video_thread = {
        let (store, stop, st) = (store.clone(), stop.clone(), st.clone());
        let fps = cfg.fps;
        std::thread::spawn(move || video_pacer(store, fps, stop, st, stdin))
    };
    let mut audio_threads = Vec::new();
    for (src, pipe) in cfg.audio.iter().zip(pipes.into_iter()) {
        let (ring, rate, ch, stop) = (src.ring.clone(), src.rate, src.channels, stop.clone());
        let label = src.label.clone();
        let st2 = st.clone();
        audio_threads.push(std::thread::spawn(move || {
            match accept_with_timeout(pipe.listener, &stop) {
                Some(conn) => audio::run_pacer(ring, rate, ch, st2.clock.clone(), stop, conn),
                None => st2.log(format!("[{label}] ffmpeg 未连接音频通道")),
            }
        }));
    }

    let exit_reason = main_loop(cfg, st, store, &stop, || {
        matches!(child.try_wait(), Ok(Some(_))).then_some("ffmpeg 提前退出")
    });
    stop.store(true, Ordering::Relaxed);
    store.stop.store(true, Ordering::Relaxed);
    let _ = video_thread.join();
    for t in audio_threads {
        let _ = t.join();
    }
    let status = child.wait()?;
    if let Ok(m) = std::fs::metadata(&cfg.output) {
        st.size.store(m.len(), Ordering::Relaxed);
    }
    if !status.success() {
        return Err(format!("ffmpeg 异常退出: {status}").into());
    }
    if exit_reason == "ffmpeg 提前退出" {
        return Err("ffmpeg 提前退出".into());
    }
    Ok(())
}

fn run_zero_copy(
    cfg: &RecordConfig,
    st: &Arc<SessionState>,
    store: &Arc<FrameStore>,
    w: u32,
    h: u32,
) -> Result<(), BoxError> {
    let (device, context, latest) = {
        let g = store.gpu.lock().unwrap();
        let g = g.as_ref().ok_or("零拷贝纹理未就绪")?;
        (g.device.0.clone(), g.context.0.clone(), g.texture.0.clone())
    };
    let pool: Vec<ID3D11Texture2D> =
        (0..POOL).map(|_| video::create_frame_texture(&device, w, h)).collect::<Result<_, _>>()?;
    let audio = (!cfg.audio.is_empty()).then(|| MfAudio { rate: 48000, channels: 2, bitrate_kbps: cfg.audio_bitrate });
    let writer = MfWriter::new(
        &device,
        &MfConfig {
            output: &cfg.output,
            width: w,
            height: h,
            fps: cfg.fps,
            codec: cfg.codec,
            quality: cfg.quality,
            audio,
        },
    )?;
    *st.encoder_name.lock().unwrap() =
        format!("MF {}（零拷贝，{:.0} Mbps）", codec_label(cfg.codec), writer.bitrate as f64 / 1e6);
    let writer = Arc::new(Mutex::new(writer));

    for s in &cfg.audio {
        s.ring.lock().unwrap().clear();
    }
    st.clock.start();
    st.set_phase(Phase::Recording);

    let stop = Arc::new(AtomicBool::new(false));
    let video_thread = {
        let (stop, st, writer) = (stop.clone(), st.clone(), writer.clone());
        let fps = cfg.fps;
        let (context, latest, pool) = (SendD3D(context), SendD3D(latest), SendD3D(pool));
        std::thread::spawn(move || gpu_pacer(context, latest, pool, writer, fps, stop, st))
    };
    let audio_thread = (!cfg.audio.is_empty()).then(|| {
        let inputs = cfg.audio.clone();
        let (stop, st, writer) = (stop.clone(), st.clone(), writer.clone());
        std::thread::spawn(move || {
            let clock = st.clock.clone();
            audio::run_mixer(inputs, 48000, 2, clock, stop, |pcm| match writer.lock().unwrap().write_audio(pcm, 2) {
                Ok(()) => true,
                Err(e) => {
                    st.log(format!("音频写入失败: {e}"));
                    false
                }
            })
        })
    });

    main_loop(cfg, st, store, &stop, || None);
    stop.store(true, Ordering::Relaxed);
    store.stop.store(true, Ordering::Relaxed);
    let video_result = video_thread.join().unwrap_or_else(|_| Err("视频线程崩溃".into()));
    if let Some(t) = audio_thread {
        let _ = t.join();
    }
    let writer = Arc::try_unwrap(writer)
        .map_err(|_| "编码器仍被占用")?
        .into_inner()
        .map_err(|_| "编码器锁损坏")?;
    writer.finish()?;
    if let Ok(m) = std::fs::metadata(&cfg.output) {
        st.size.store(m.len(), Ordering::Relaxed);
    }
    video_result?;
    Ok(())
}
