//! screenrec-interp 命令行：显卡插帧（RIFE），把视频帧率提高到指定值或指定倍数。

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

use screenrec::encoder::{self, Backend, Codec};
use screenrec::process::{self, FpsTarget, InterpOpts, JobSpec, Op};
use screenrec::BoxError;

#[derive(Parser, Debug)]
#[command(
    name = "screenrec-interp",
    version,
    about = "显卡插帧：用 RIFE 把视频帧率提高到指定值或指定倍数",
    after_help = "示例:\n  screenrec-interp in.mp4                 帧率翻倍\n  screenrec-interp in.mp4 --fps 120       30 帧变 120 帧\n  screenrec-interp in.mp4 --times 4       帧率 ×4\n  screenrec-interp in.mp4 --fps 60 --start 1:00 --end 1:30   只处理一段"
)]
struct Cli {
    /// 源视频
    input: PathBuf,
    /// 输出文件（默认在源文件旁边，名字带目标帧率）
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// 目标帧率（如 60、120、59.94）
    #[arg(long, conflicts_with = "times")]
    fps: Option<f64>,
    /// 帧率倍数（默认 2）
    #[arg(long)]
    times: Option<u32>,
    /// 插帧模型（rife 目录下的子目录名）
    #[arg(long, default_value = "rife-v4.6")]
    model: String,
    /// 显卡编号（多显卡时 0 通常是独显；填 -1 让 rife 自动选择）
    #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
    gpu: i32,
    /// 视频编码格式
    #[arg(long, value_enum, default_value_t = Codec::H264)]
    codec: Codec,
    /// 编码后端（auto 按 NVENC > QSV > AMF > CPU 探测）
    #[arg(long, value_enum, default_value_t = Backend::Auto)]
    encoder: Backend,
    /// 画质：CQ/CRF 值，越小越好
    #[arg(short, long, default_value_t = 18)]
    quality: u32,
    /// 只处理片段：起点（秒 / 分:秒 / 时:分:秒）
    #[arg(long)]
    start: Option<String>,
    /// 只处理片段：终点
    #[arg(long)]
    end: Option<String>,
    /// ffmpeg 可执行文件路径（默认找同目录或 PATH）
    #[arg(long)]
    ffmpeg: Option<PathBuf>,
    /// rife-ncnn-vulkan 路径（默认找程序目录的 rife 文件夹）
    #[arg(long)]
    rife: Option<PathBuf>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("\n错误: {e}");
        std::process::exit(1);
    }
}

fn parse_time(arg: &Option<String>, what: &str) -> Result<Option<f64>, BoxError> {
    match arg {
        None => Ok(None),
        Some(s) => process::parse_time(s).map(Some).ok_or_else(|| format!("{what}时间格式不对: {s}").into()),
    }
}

fn run() -> Result<(), BoxError> {
    let cli = Cli::parse();
    if !cli.input.exists() {
        return Err(format!("源视频不存在: {}", cli.input.display()).into());
    }
    let ffmpeg = encoder::find_ffmpeg(cli.ffmpeg.as_deref())?;
    let rife = process::find_rife(cli.rife.as_deref())?;
    let target = match (cli.fps, cli.times) {
        (Some(f), _) if f > 0.0 => FpsTarget::Fps(f),
        (Some(_), _) => return Err("--fps 必须大于 0".into()),
        (None, t) => FpsTarget::Times(t.unwrap_or(2).max(2)),
    };
    let info = process::probe(&ffmpeg, &cli.input)?;
    let (tn, td) = process::target_fps(info.fps, target);
    let op = Op::Interp(InterpOpts {
        target,
        model: cli.model.clone(),
        gpu: u32::try_from(cli.gpu).ok(),
        codec: cli.codec,
        backend: cli.encoder,
        quality: cli.quality,
    });
    let dst = cli.output.clone().unwrap_or_else(|| process::default_dst(&cli.input, &op, Some(&info)));
    eprintln!(
        "源: {}x{} @ {:.3} fps，时长 {:.1} 秒 -> 目标 {:.3} fps",
        info.width,
        info.height,
        info.fps_f64(),
        info.duration,
        tn as f64 / td as f64
    );
    let job = process::start(JobSpec {
        src: cli.input.clone(),
        dst: dst.clone(),
        start: parse_time(&cli.start, "起点")?.unwrap_or(0.0),
        end: parse_time(&cli.end, "终点")?,
        op,
        ffmpeg,
        rife: Some(rife),
    });
    {
        let j = job.clone();
        ctrlc::set_handler(move || j.cancel())?;
    }
    let result = loop {
        if let Some(r) = job.result() {
            break r;
        }
        let p = job.progress() as f64;
        let used = job.started.elapsed().as_secs_f64();
        let eta = if p > 0.01 { format!("剩余约 {:.0} 秒", used * (1.0 - p) / p) } else { String::new() };
        eprint!("\r{:5.1}%  {}  {eta}          ", p * 100.0, job.stage());
        std::thread::sleep(Duration::from_millis(300));
    };
    eprintln!();
    result.map_err(|e| -> BoxError { e.into() })?;
    let size = std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "完成: {}  用时 {:.0} 秒  大小 {:.1} MB",
        dst.display(),
        job.started.elapsed().as_secs_f64(),
        size as f64 / 1048576.0
    );
    Ok(())
}
