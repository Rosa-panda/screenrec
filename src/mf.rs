//! Media Foundation 零拷贝编码：BGRA 纹理直接交给硬件编码器（Sink Writer），
//! 画面全程不经过 CPU；音频由本进程混成一路 PCM 一并写入 mp4。

use std::path::Path;

use windows::core::{Interface, GUID, HSTRING};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Variant::{VARIANT, VT_UI4};

use crate::encoder::Codec;
use crate::video::BoxError;

pub struct MfAudio {
    pub rate: u32,
    pub channels: u16,
    pub bitrate_kbps: u32,
}

pub struct MfConfig<'a> {
    pub output: &'a Path,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: Codec,
    pub quality: u32,
    pub audio: Option<MfAudio>,
}

pub struct MfWriter {
    writer: IMFSinkWriter,
    _manager: IMFDXGIDeviceManager,
    video_stream: u32,
    audio_stream: Option<u32>,
    fps: u32,
    audio_rate: u32,
    audio_frames: u64,
    pub bitrate: u32,
}

// Sink Writer 内部自带锁；本结构只在 Mutex 里跨线程使用
unsafe impl Send for MfWriter {}

/// 由 CQ 档位换算 MF 编码器的目标码率（bit/s）：CQ 18 在 2560x1600@60 约 33 Mbps。
pub fn bitrate_for(w: u32, h: u32, fps: u32, quality: u32, codec: Codec) -> u32 {
    let bpp = 0.9 * 0.9f64.powi(quality as i32);
    let mut br = w as f64 * h as f64 * fps as f64 * bpp;
    if codec == Codec::Hevc {
        br *= 0.65;
    }
    br.clamp(2_000_000.0, 200_000_000.0) as u32
}

fn pack(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

fn var_u32(v: u32) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        (*var.Anonymous.Anonymous).vt = VT_UI4;
        (*var.Anonymous.Anonymous).Anonymous.ulVal = v;
    }
    var
}

impl MfWriter {
    pub fn new(device: &ID3D11Device, cfg: &MfConfig) -> Result<Self, BoxError> {
        let subtype = match cfg.codec {
            Codec::H264 => MFVideoFormat_H264,
            Codec::Hevc => MFVideoFormat_HEVC,
            Codec::Av1 => return Err("零拷贝引擎暂不支持 AV1，请选 H.264 或 HEVC".into()),
        };
        let bitrate = bitrate_for(cfg.width, cfg.height, cfg.fps, cfg.quality, cfg.codec);
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)?;

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.ok_or("MFCreateDXGIDeviceManager 返回空")?;
            manager.ResetDevice(device, token)?;

            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 4)?;
            let attrs = attrs.ok_or("MFCreateAttributes 返回空")?;
            attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &manager)?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;

            let url = HSTRING::from(cfg.output.as_os_str());
            let writer = MFCreateSinkWriterFromURL(&url, None, &attrs)?;

            // ---- 视频输出类型 ----
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, pack(cfg.width, cfg.height))?;
            out.SetUINT64(&MF_MT_FRAME_RATE, pack(cfg.fps, 1))?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            let profile = match cfg.codec {
                Codec::H264 => eAVEncH264VProfile_High.0 as u32,
                _ => eAVEncH265VProfile_Main_420_8.0 as u32,
            };
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
            out.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            out.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            out.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
            out.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
            let video_stream = writer.AddStream(&out)?;

            // ---- 视频输入类型：BGRA 纹理 ----
            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_ARGB32)?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(cfg.width, cfg.height))?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, pack(cfg.fps, 1))?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            inp.SetUINT32(&MF_MT_DEFAULT_STRIDE, cfg.width * 4)?;
            inp.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
            inp.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_0_255.0 as u32)?;
            inp.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            inp.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
            inp.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
            writer
                .SetInputMediaType(video_stream, &inp, None)
                .map_err(|e| format!("硬件编码器不接受 BGRA 纹理输入: {e}"))?;

            // ---- 编码器细节（失败不致命）----
            let mut raw = std::ptr::null_mut();
            if writer
                .GetServiceForStream(video_stream, &GUID::zeroed(), &ICodecAPI::IID, &mut raw)
                .is_ok()
                && !raw.is_null()
            {
                let api = ICodecAPI::from_raw(raw);
                let _ = api.SetValue(
                    &CODECAPI_AVEncCommonRateControlMode,
                    &var_u32(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32),
                );
                let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &var_u32(bitrate));
                let _ = api.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &var_u32(bitrate.saturating_mul(2)));
                let _ = api.SetValue(&CODECAPI_AVEncMPVGOPSize, &var_u32(cfg.fps * 2));
                let _ = api.SetValue(&CODECAPI_AVEncCommonQualityVsSpeed, &var_u32(100));
            }

            // ---- 音频：PCM16 输入 → AAC ----
            let mut audio_stream = None;
            let mut audio_rate = 0;
            if let Some(a) = &cfg.audio {
                let ch = a.channels as u32;
                let want = a.bitrate_kbps * 125; // bytes/s
                let mut stream = None;
                // 微软 AAC 编码器只接受若干固定码率，先试请求值，不行再退到 192k
                for bps in [want, 24000] {
                    let out = MFCreateMediaType()?;
                    out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                    out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
                    out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                    out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, a.rate)?;
                    out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, ch)?;
                    out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bps)?;
                    if let Ok(idx) = writer.AddStream(&out) {
                        let inp = MFCreateMediaType()?;
                        inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                        inp.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
                        inp.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                        inp.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, a.rate)?;
                        inp.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, ch)?;
                        inp.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, ch * 2)?;
                        inp.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, a.rate * ch * 2)?;
                        inp.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
                        if writer.SetInputMediaType(idx, &inp, None).is_ok() {
                            stream = Some(idx);
                            break;
                        }
                    }
                }
                audio_stream = Some(stream.ok_or("AAC 音频编码器初始化失败")?);
                audio_rate = a.rate;
            }

            writer.BeginWriting()?;
            Ok(Self {
                writer,
                _manager: manager,
                video_stream,
                audio_stream,
                fps: cfg.fps,
                audio_rate,
                audio_frames: 0,
                bitrate,
            })
        }
    }

    /// 写入第 `index` 帧（纹理必须来自创建时传入的设备）。
    pub fn write_video(&self, tex: &ID3D11Texture2D, index: u64) -> Result<(), BoxError> {
        unsafe {
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
            let len = buf.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
            buf.SetCurrentLength(len)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime((index * 10_000_000 / self.fps as u64) as i64)?;
            sample.SetSampleDuration((10_000_000 / self.fps) as i64)?;
            self.writer.WriteSample(self.video_stream, &sample)?;
        }
        Ok(())
    }

    /// 写入一段交错 PCM16，时间戳按累计采样数推进。
    pub fn write_audio(&mut self, pcm: &[i16], channels: u16) -> Result<(), BoxError> {
        let Some(stream) = self.audio_stream else { return Ok(()) };
        if pcm.is_empty() {
            return Ok(());
        }
        let bytes = pcm.len() * 2;
        let frames = (pcm.len() / channels as usize) as u64;
        unsafe {
            let buf = MFCreateMemoryBuffer(bytes as u32)?;
            let mut p = std::ptr::null_mut();
            buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr() as *const u8, p, bytes);
            buf.Unlock()?;
            buf.SetCurrentLength(bytes as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            let rate = self.audio_rate as u64;
            sample.SetSampleTime((self.audio_frames * 10_000_000 / rate) as i64)?;
            sample.SetSampleDuration((frames * 10_000_000 / rate) as i64)?;
            self.writer.WriteSample(stream, &sample)?;
        }
        self.audio_frames += frames;
        Ok(())
    }

    /// 写文件尾。
    pub fn finish(self) -> Result<(), BoxError> {
        unsafe { self.writer.Finalize()? };
        Ok(())
    }
}
