//! screenrec 命令行：参数解析 + 状态显示，录制逻辑在 `screenrec::recorder`。

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use clap::Parser;
use windows_capture::monitor::Monitor;
use windows_capture::window::Window;

use screenrec::encoder::{self, Backend, Codec};
use screenrec::recorder::{Engine, Phase, RecordConfig, Session};
use screenrec::video::Target;
use screenrec::{audio, BoxError};

#[derive(Parser, Debug)]
#[command(
    name = "screenrec",
    version,
    about = "Windows 高质量录屏：抓屏 + 系统声音 + 麦克风，ffmpeg 硬件编码",
    after_help = "示例:\n  screenrec                      录主显示器，Ctrl+C 停止\n  screenrec -m 2 -o demo.mp4     录第 2 个显示器\n  screenrec -w \"Chrome\"          录标题含 Chrome 的窗口\n  screenrec --codec hevc -q 16   HEVC、更高画质\n  screenrec --engine game        游戏模式（低占用，录游戏推荐）\n  screenrec --engine zero-copy   零拷贝引擎（占用最低）\n  screenrec --separate-tracks    系统声音与麦克风分成两条音轨\n  screenrec --list               列出显示器 / 窗口 / 音频设备"
)]
struct Cli {
    /// 输出文件（默认 rec_YYYYMMDD_HHMMSS.mp4）
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// 录制第 N 个显示器（从 1 起，见 --list），默认主显示器
    #[arg(short, long)]
    monitor: Option<usize>,
    /// 录制标题包含该文字的窗口
    #[arg(short, long, conflicts_with = "monitor")]
    window: Option<String>,
    /// 列出显示器 / 窗口 / 音频设备后退出
    #[arg(long)]
    list: bool,
    /// 录制帧率
    #[arg(long, default_value_t = 60)]
    fps: u32,
    /// 视频编码格式
    #[arg(long, value_enum, default_value_t = Codec::H264)]
    codec: Codec,
    /// 编码后端（auto 按 NVENC > QSV > AMF > CPU 探测）
    #[arg(long, value_enum, default_value_t = Backend::Auto)]
    encoder: Backend,
    /// 画质：CQ/CRF 值，越小越好（推荐 15-23）
    #[arg(short, long, default_value_t = 18)]
    quality: u32,
    /// 录制引擎：standard 标准 / game 游戏（低占用）/ zero-copy 零拷贝（占用最低，仅 mp4 + H.264/HEVC）
    #[arg(long, value_enum, default_value_t = Engine::Standard)]
    engine: Engine,
    /// 不录系统内部声音
    #[arg(long)]
    no_system_audio: bool,
    /// 不录麦克风
    #[arg(long)]
    no_mic: bool,
    /// 麦克风设备名称子串（默认用系统默认麦克风）
    #[arg(long)]
    mic: Option<String>,
    /// 麦克风音量倍数
    #[arg(long, default_value_t = 1.0)]
    mic_volume: f32,
    /// 系统声音音量倍数
    #[arg(long, default_value_t = 1.0)]
    system_volume: f32,
    /// 系统声音与麦克风分别保存为两条音轨（默认混成一条）
    #[arg(long)]
    separate_tracks: bool,
    /// AAC 音频码率（kbps）
    #[arg(long, default_value_t = 256)]
    audio_bitrate: u32,
    /// 不录鼠标指针
    #[arg(long)]
    no_cursor: bool,
    /// 录制时长（秒）；不填则按 Ctrl+C 停止
    #[arg(short, long)]
    duration: Option<f64>,
    /// ffmpeg 可执行文件路径（默认找同目录或 PATH）
    #[arg(long)]
    ffmpeg: Option<PathBuf>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("\n错误: {e}");
        std::process::exit(1);
    }
}

fn list_all() -> Result<(), BoxError> {
    println!("显示器:");
    for m in Monitor::enumerate()? {
        let idx = m.index().unwrap_or(0);
        let name = m.name().unwrap_or_else(|_| "?".into());
        let (w, h) = (m.width().unwrap_or(0), m.height().unwrap_or(0));
        let hz = m.refresh_rate().unwrap_or(0);
        println!("  [{idx}] {name}  {w}x{h} @ {hz}Hz");
    }
    println!("窗口（-w 用标题子串匹配）:");
    for w in Window::enumerate()? {
        if let Ok(t) = w.title() {
            if !t.trim().is_empty() {
                println!("  - {t}");
            }
        }
    }
    audio::list_devices(&cpal::default_host());
    Ok(())
}

fn fmt_time(secs: f64) -> String {
    let s = secs as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

fn run() -> Result<(), BoxError> {
    let cli = Cli::parse();
    if cli.list {
        return list_all();
    }
    if cli.fps == 0 || cli.fps > 240 {
        return Err("fps 需在 1..=240".into());
    }
    let output = cli.output.clone().unwrap_or_else(|| {
        PathBuf::from(format!("rec_{}.mp4", chrono::Local::now().format("%Y%m%d_%H%M%S")))
    });
    let ffmpeg = encoder::find_ffmpeg(cli.ffmpeg.as_deref())?;

    // ---- 目标 ----
    let target = if let Some(title) = &cli.window {
        let w = Window::from_contains_name(title)
            .map_err(|e| format!("找不到标题包含 \"{title}\" 的窗口: {e:?}"))?;
        eprintln!("录制窗口: {}", w.title().unwrap_or_default());
        Target::Window(w)
    } else {
        let m = match cli.monitor {
            Some(i) => Monitor::from_index(i).map_err(|e| format!("显示器 {i} 不存在: {e:?}"))?,
            None => Monitor::primary()?,
        };
        eprintln!(
            "录制显示器: [{}] {}",
            m.index().unwrap_or(0),
            m.name().unwrap_or_else(|_| "?".into())
        );
        Target::Monitor(m)
    };

    // ---- 音频 ----
    let host = cpal::default_host();
    let mut sources = Vec::new();
    if !cli.no_system_audio {
        match audio::open_system_audio(&host, None) {
            Ok(s) => {
                eprintln!("系统声音: {} ({} Hz, {} ch)", s.device_name, s.rate, s.channels);
                sources.push((s, cli.system_volume));
            }
            Err(e) => eprintln!("系统声音不可用，跳过: {e}"),
        }
    }
    if !cli.no_mic {
        match audio::open_mic(&host, cli.mic.as_deref()) {
            Ok(s) => {
                eprintln!("麦克风: {} ({} Hz, {} ch)", s.device_name, s.rate, s.channels);
                sources.push((s, cli.mic_volume));
            }
            Err(e) => eprintln!("麦克风不可用，跳过: {e}"),
        }
    }
    let audio_inputs = sources.iter().map(|(s, v)| s.input(*v)).collect();

    // ---- 会话 ----
    let session = Session::start(RecordConfig {
        output: output.clone(),
        target,
        cursor: !cli.no_cursor,
        fps: cli.fps,
        codec: cli.codec,
        backend: cli.encoder,
        quality: cli.quality,
        audio_bitrate: cli.audio_bitrate,
        separate_tracks: cli.separate_tracks,
        audio: audio_inputs,
        duration: cli.duration,
        ffmpeg,
        engine: cli.engine,
        hide_console: false,
        echo_ffmpeg: true,
    });
    {
        let st = session.state.clone();
        ctrlc::set_handler(move || st.request_stop())?;
    }

    let st = session.state.clone();
    let mut announced = false;
    loop {
        match st.phase() {
            Phase::Starting => eprint!("\r准备中（探测编码器 / 抓取首帧）...   "),
            Phase::Recording => {
                if !announced {
                    announced = true;
                    let (w, h) = *st.frame_size.lock().unwrap();
                    eprintln!(
                        "\r画面尺寸: {w}x{h} @ {} fps，编码 {}，CQ/CRF {}          ",
                        cli.fps,
                        st.encoder_name.lock().unwrap(),
                        cli.quality
                    );
                    eprintln!("开始录制 -> {}   （Ctrl+C 停止）", output.display());
                }
                eprint!(
                    "\r● REC {}  已编码 {} 帧 / 抓取 {} 帧  {:.1} MB   ",
                    fmt_time(st.elapsed()),
                    st.sent.load(Ordering::Relaxed),
                    st.captured.load(Ordering::Relaxed),
                    st.size.load(Ordering::Relaxed) as f64 / 1048576.0
                );
            }
            Phase::Finishing => eprint!("\r停止中，等待编码器写完...                               "),
            Phase::Done | Phase::Failed => break,
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!();
    let elapsed = st.elapsed();
    session.wait().map_err(|e| -> BoxError { e.into() })?;
    let size = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "完成（{}）: {}  时长 {}  大小 {:.1} MB",
        st.exit_reason.lock().unwrap(),
        output.display(),
        fmt_time(elapsed),
        size as f64 / 1048576.0
    );
    drop(sources);
    Ok(())
}
