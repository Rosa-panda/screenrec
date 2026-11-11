//! 音频抓取：系统内部声音（WASAPI loopback）与麦克风，统一转成 f32 交错采样放进环形缓冲。
//! 配速线程按墙钟严格输出 rate × 时间 个采样（不足补零，积压过多丢弃），保证音画同步。

use std::collections::VecDeque;
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, SupportedStreamConfig};

use crate::clock::Clock;
use crate::video::BoxError;

pub type Ring = Arc<Mutex<VecDeque<f32>>>;

/// 一路已打开的音频流（持有 cpal Stream，丢弃即停止）。
pub struct AudioSource {
    pub label: String,
    pub device_name: String,
    pub rate: u32,
    pub channels: u16,
    pub ring: Ring,
    /// 最近一段数据的峰值（f32 bits），UI 电平表用；读取方可用 swap(0) 取走
    pub level: Arc<AtomicU32>,
    _stream: cpal::Stream,
}

/// 交给录制会话的轻量句柄（不含 Stream，可跨线程）。
#[derive(Clone)]
pub struct AudioInput {
    pub label: String,
    pub rate: u32,
    pub channels: u16,
    pub ring: Ring,
    pub volume: f32,
}

impl AudioSource {
    pub fn input(&self, volume: f32) -> AudioInput {
        AudioInput {
            label: self.label.clone(),
            rate: self.rate,
            channels: self.channels,
            ring: self.ring.clone(),
            volume,
        }
    }

    /// 取走并清零当前峰值（0.0 ~ 1.0+）。
    pub fn take_level(&self) -> f32 {
        f32::from_bits(self.level.swap(0, Ordering::Relaxed))
    }
}

pub fn device_name(dev: &cpal::Device) -> String {
    dev.description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unknown>".into())
}

pub fn output_device_names(host: &cpal::Host) -> Vec<String> {
    host.output_devices()
        .map(|it| it.map(|d| device_name(&d)).collect())
        .unwrap_or_default()
}

pub fn input_device_names(host: &cpal::Host) -> Vec<String> {
    host.input_devices()
        .map(|it| it.map(|d| device_name(&d)).collect())
        .unwrap_or_default()
}

pub fn default_output_name(host: &cpal::Host) -> Option<String> {
    host.default_output_device().map(|d| device_name(&d))
}

pub fn default_input_name(host: &cpal::Host) -> Option<String> {
    host.default_input_device().map(|d| device_name(&d))
}

/// 系统内部声音：把输出设备当作输入设备打开，cpal 会自动启用 loopback。
/// `name` 为 None 时用默认输出设备，否则按名称子串匹配。
pub fn open_system_audio(host: &cpal::Host, name: Option<&str>) -> Result<AudioSource, BoxError> {
    let dev = match name {
        Some(n) => {
            let n = n.to_lowercase();
            host.output_devices()?
                .find(|d| device_name(d).to_lowercase().contains(&n))
                .ok_or_else(|| format!("没有找到名称包含 \"{n}\" 的输出设备"))?
        }
        None => host
            .default_output_device()
            .ok_or("没有找到默认音频输出设备")?,
    };
    let cfg = dev.default_output_config()?;
    build(dev, cfg, "系统声音")
}

/// 麦克风：默认输入设备，或按名称子串匹配。
pub fn open_mic(host: &cpal::Host, name: Option<&str>) -> Result<AudioSource, BoxError> {
    let dev = match name {
        Some(n) => {
            let n = n.to_lowercase();
            host.input_devices()?
                .find(|d| device_name(d).to_lowercase().contains(&n))
                .ok_or_else(|| format!("没有找到名称包含 \"{n}\" 的输入设备"))?
        }
        None => host
            .default_input_device()
            .ok_or("没有默认麦克风设备")?,
    };
    let cfg = dev.default_input_config()?;
    build(dev, cfg, "麦克风").map_err(|e| {
        let msg = e.to_string();
        if msg.contains("Access is denied") || msg.contains("拒绝访问") {
            format!("{msg}\n  提示：Windows 已禁止应用使用麦克风。请到 设置 > 隐私和安全性 > 麦克风，打开“麦克风访问”和“允许桌面应用访问麦克风”").into()
        } else {
            e
        }
    })
}

fn build(dev: cpal::Device, cfg: SupportedStreamConfig, label: &str) -> Result<AudioSource, BoxError> {
    let name = device_name(&dev);
    let ring: Ring = Arc::new(Mutex::new(VecDeque::new()));
    let level = Arc::new(AtomicU32::new(0));
    let rate = cfg.sample_rate();
    let channels = cfg.channels();
    // 环形缓冲最多保留 2 秒，超过说明消费端卡住，丢弃最旧数据
    let cap = (rate as usize) * (channels as usize) * 2;
    let stream = match cfg.sample_format() {
        SampleFormat::F32 => build_typed::<f32>(&dev, &cfg, ring.clone(), level.clone(), cap, label)?,
        SampleFormat::I16 => build_typed::<i16>(&dev, &cfg, ring.clone(), level.clone(), cap, label)?,
        SampleFormat::I32 => build_typed::<i32>(&dev, &cfg, ring.clone(), level.clone(), cap, label)?,
        SampleFormat::U16 => build_typed::<u16>(&dev, &cfg, ring.clone(), level.clone(), cap, label)?,
        SampleFormat::U8 => build_typed::<u8>(&dev, &cfg, ring.clone(), level.clone(), cap, label)?,
        other => return Err(format!("{label} 采样格式 {other:?} 不支持").into()),
    };
    stream.play()?;
    Ok(AudioSource {
        label: label.to_string(),
        device_name: name,
        rate,
        channels,
        ring,
        level,
        _stream: stream,
    })
}

fn build_typed<T>(
    dev: &cpal::Device,
    cfg: &SupportedStreamConfig,
    ring: Ring,
    level: Arc<AtomicU32>,
    cap: usize,
    label: &str,
) -> Result<cpal::Stream, BoxError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let label = label.to_string();
    let stream = dev.build_input_stream::<T, _, _>(
        cfg.config(),
        move |data, _| {
            let mut peak = f32::from_bits(level.load(Ordering::Relaxed));
            let mut r = ring.lock().unwrap();
            for s in data {
                let v = f32::from_sample(*s);
                let a = v.abs();
                if a > peak {
                    peak = a;
                }
                r.push_back(v);
            }
            let len = r.len();
            if len > cap {
                r.drain(..len - cap);
            }
            drop(r);
            level.store(peak.to_bits(), Ordering::Relaxed);
        },
        move |e| {
            // WASAPI 在流刚启动或 loopback 源静音时会报一次数据不连续，无害，不打扰用户
            let msg = e.to_string();
            if !msg.contains("underrun or overrun") {
                eprintln!("\n[{label}] 音频流错误: {msg}");
            }
        },
        None,
    )?;
    Ok(stream)
}

/// 把一路音频按墙钟配速写到 ffmpeg（f32le 交错）。
pub fn run_pacer(
    ring: Ring,
    rate: u32,
    channels: u16,
    clock: Arc<Clock>,
    stop: Arc<AtomicBool>,
    mut out: TcpStream,
) {
    let ch = channels as usize;
    let mut sent_frames: u64 = 0;
    let mut bytes: Vec<u8> = Vec::new();
    let tick = Duration::from_millis(10);
    loop {
        let stopping = stop.load(Ordering::Relaxed);
        if clock.is_paused() {
            // 暂停期间的声音不进文件
            ring.lock().unwrap().clear();
        }
        let target = (clock.elapsed() * rate as f64) as u64;
        let need = target.saturating_sub(sent_frames) as usize;
        if need > 0 {
            bytes.clear();
            bytes.reserve(need * ch * 4);
            {
                let mut r = ring.lock().unwrap();
                let avail = r.len() / ch;
                let take = avail.min(need);
                for s in r.drain(..take * ch) {
                    bytes.extend_from_slice(&s.to_le_bytes());
                }
            }
            // 不足的部分补静音
            bytes.resize(need * ch * 4, 0);
            if out.write_all(&bytes).is_err() {
                break;
            }
            sent_frames = target;
        }
        if stopping {
            break;
        }
        std::thread::sleep(tick);
    }
    let _ = out.flush();
    let _ = out.shutdown(std::net::Shutdown::Both);
}

struct MixSrc {
    input: AudioInput,
    /// 输入采样率 / 输出采样率
    ratio: f64,
    /// 当前输出位置对应 `buf` 中的小数帧索引
    pos: f64,
    /// 从环形缓冲取出、尚未消费完的交错输入帧
    buf: Vec<f32>,
}

/// 把多路输入按墙钟配速、重采样并混成一路交错 PCM16（零拷贝引擎用）。
/// 每 10ms 调一次 `sink`，返回 false 则结束。
pub fn run_mixer(
    inputs: Vec<AudioInput>,
    out_rate: u32,
    out_ch: usize,
    clock: Arc<Clock>,
    stop: Arc<AtomicBool>,
    mut sink: impl FnMut(&[i16]) -> bool,
) {
    let mut srcs: Vec<MixSrc> = inputs
        .into_iter()
        .map(|input| MixSrc { ratio: input.rate as f64 / out_rate as f64, input, pos: 0.0, buf: Vec::new() })
        .collect();
    let mut sent: u64 = 0;
    let mut mix: Vec<f32> = Vec::new();
    let mut out: Vec<i16> = Vec::new();
    let tick = Duration::from_millis(10);
    loop {
        let stopping = stop.load(Ordering::Relaxed);
        if clock.is_paused() {
            // 暂停期间的声音不进文件
            for s in &srcs {
                s.input.ring.lock().unwrap().clear();
            }
        }
        let target = (clock.elapsed() * out_rate as f64) as u64;
        let need = target.saturating_sub(sent) as usize;
        if need > 0 {
            mix.clear();
            mix.resize(need * out_ch, 0.0);
            for s in &mut srcs {
                let ch = s.input.channels as usize;
                let span = s.pos + need as f64 * s.ratio;
                // 线性插值要用到 floor(span)+1 帧，再多留一帧余量
                let want_frames = span.ceil() as usize + 2;
                let have = s.buf.len() / ch;
                if want_frames > have {
                    let missing = want_frames - have;
                    let mut r = s.input.ring.lock().unwrap();
                    let take = (r.len() / ch).min(missing);
                    s.buf.extend(r.drain(..take * ch));
                    drop(r);
                    // 不足的部分补静音
                    s.buf.resize(s.buf.len() + (missing - take) * ch, 0.0);
                }
                let vol = s.input.volume;
                for i in 0..need {
                    let p = s.pos + i as f64 * s.ratio;
                    let i0 = p as usize;
                    let f = (p - i0 as f64) as f32;
                    for c in 0..out_ch {
                        let sc = if ch == 1 { 0 } else { c.min(ch - 1) };
                        let a = s.buf[i0 * ch + sc];
                        let b = s.buf[(i0 + 1) * ch + sc];
                        mix[i * out_ch + c] += (a + (b - a) * f) * vol;
                    }
                }
                let consumed = span as usize;
                s.buf.drain(..consumed * ch);
                s.pos = span - consumed as f64;
            }
            out.clear();
            out.extend(mix.iter().map(|v| (v.clamp(-1.0, 1.0) * 32767.0) as i16));
            if !sink(&out) {
                break;
            }
            sent = target;
        }
        if stopping {
            break;
        }
        std::thread::sleep(tick);
    }
}

/// 列出音频设备（给 --list 用）。
pub fn list_devices(host: &cpal::Host) {
    println!("音频输出设备（系统声音会从默认输出设备 loopback 录制）:");
    let def_out = default_output_name(host);
    for n in output_device_names(host) {
        let mark = if Some(&n) == def_out.as_ref() { " [默认]" } else { "" };
        println!("  - {n}{mark}");
    }
    println!("音频输入设备（麦克风，--mic <名称子串> 选择）:");
    let def_in = default_input_name(host);
    for n in input_device_names(host) {
        let mark = if Some(&n) == def_in.as_ref() { " [默认]" } else { "" };
        println!("  - {n}{mark}");
    }
}
