//! ffmpeg 定位、编码器探测与命令行拼装。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::video::BoxError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
pub enum Backend {
    Auto,
    Nvenc,
    Qsv,
    Amf,
    Cpu,
}

pub fn find_ffmpeg(explicit: Option<&Path>) -> Result<PathBuf, BoxError> {
    if let Some(p) = explicit {
        if p.exists() {
            return Ok(p.to_path_buf());
        }
        return Err(format!("指定的 ffmpeg 不存在: {}", p.display()).into());
    }
    // 1. 可执行文件同目录
    if let Ok(exe) = std::env::current_exe() {
        let p = exe.with_file_name("ffmpeg.exe");
        if p.exists() {
            return Ok(p);
        }
    }
    // 2. PATH
    if Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return Ok(PathBuf::from("ffmpeg"));
    }
    Err("找不到 ffmpeg：请安装并加入 PATH，或用 --ffmpeg 指定路径".into())
}

fn encoder_name(codec: Codec, backend: Backend) -> &'static str {
    match (codec, backend) {
        (Codec::H264, Backend::Nvenc) => "h264_nvenc",
        (Codec::Hevc, Backend::Nvenc) => "hevc_nvenc",
        (Codec::Av1, Backend::Nvenc) => "av1_nvenc",
        (Codec::H264, Backend::Qsv) => "h264_qsv",
        (Codec::Hevc, Backend::Qsv) => "hevc_qsv",
        (Codec::Av1, Backend::Qsv) => "av1_qsv",
        (Codec::H264, Backend::Amf) => "h264_amf",
        (Codec::Hevc, Backend::Amf) => "hevc_amf",
        (Codec::Av1, Backend::Amf) => "av1_amf",
        (Codec::H264, Backend::Cpu) => "libx264",
        (Codec::Hevc, Backend::Cpu) => "libx265",
        (Codec::Av1, Backend::Cpu) => "libsvtav1",
        (_, Backend::Auto) => unreachable!(),
    }
}

/// 真实跑一次 3 帧的测试编码，确认驱动/硬件可用。
fn probe(ffmpeg: &Path, enc: &str) -> bool {
    Command::new(ffmpeg)
        .args([
            "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
            "color=c=black:s=256x256:r=30", "-frames:v", "3", "-pix_fmt", "yuv420p",
            "-c:v", enc, "-f", "null", "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 选定实际使用的后端（Auto 时按 NVENC > QSV > AMF > CPU 顺序探测）。
pub fn pick_backend(ffmpeg: &Path, codec: Codec, want: Backend) -> Result<(Backend, &'static str), BoxError> {
    let candidates: Vec<Backend> = match want {
        Backend::Auto => vec![Backend::Nvenc, Backend::Qsv, Backend::Amf, Backend::Cpu],
        b => vec![b],
    };
    for b in candidates {
        let enc = encoder_name(codec, b);
        if probe(ffmpeg, enc) {
            return Ok((b, enc));
        }
    }
    Err(format!("没有可用的 {codec:?} 编码器（后端 {want:?}）").into())
}

/// 编码器参数。quality 是 CQ/CRF 值，越小质量越高；game 为游戏模式（更轻的编码预设）。
pub fn video_args(codec: Codec, backend: Backend, enc: &str, quality: u32, game: bool) -> Vec<String> {
    let q = quality.to_string();
    let mut a: Vec<String> = vec!["-c:v".into(), enc.into()];
    let push = |a: &mut Vec<String>, items: &[&str]| a.extend(items.iter().map(|s| s.to_string()));
    match backend {
        Backend::Nvenc => {
            if game {
                // p4、无 lookahead/时域 AQ：编码引擎负载约减半，观感差别很小
                push(&mut a, &["-preset", "p4", "-tune", "hq", "-rc", "vbr", "-cq", &q, "-b:v", "0", "-spatial-aq", "1"]);
            } else {
                push(&mut a, &[
                    "-preset", "p6", "-tune", "hq", "-rc", "vbr", "-cq", &q, "-b:v", "0",
                    "-spatial-aq", "1", "-temporal-aq", "1", "-rc-lookahead", "16",
                ]);
            }
            if codec == Codec::H264 {
                push(&mut a, &["-profile:v", "high"]);
            }
        }
        Backend::Qsv => push(&mut a, &["-preset", "slower", "-global_quality", &q]),
        Backend::Amf => push(&mut a, &["-quality", "quality", "-rc", "cqp", "-qp_i", &q, "-qp_p", &q]),
        Backend::Cpu => match codec {
            Codec::H264 => push(&mut a, &["-preset", "faster", "-crf", &q, "-profile:v", "high"]),
            Codec::Hevc => push(&mut a, &["-preset", "fast", "-crf", &q]),
            Codec::Av1 => push(&mut a, &["-preset", "8", "-crf", &q]),
        },
        Backend::Auto => unreachable!(),
    }
    if codec == Codec::Hevc {
        push(&mut a, &["-tag:v", "hvc1"]);
    }
    push(&mut a, &[
        "-color_range", "tv", "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709",
    ]);
    // 硬件编码器常常不写 VUI 色彩信息，用码流滤镜强制写入 BT.709，播放器才能正确还原颜色
    let bsf = match codec {
        Codec::H264 => "h264_metadata=colour_primaries=1:transfer_characteristics=1:matrix_coefficients=1:video_full_range_flag=0",
        Codec::Hevc => "hevc_metadata=colour_primaries=1:transfer_characteristics=1:matrix_coefficients=1:video_full_range_flag=0",
        Codec::Av1 => "av1_metadata=color_primaries=1:transfer_characteristics=1:matrix_coefficients=1:color_range=0",
    };
    push(&mut a, &["-bsf:v", bsf]);
    a
}
