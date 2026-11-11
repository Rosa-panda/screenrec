//! 视频抓取：基于 Windows Graphics Capture。
//!
//! - 标准/游戏引擎：把最新一帧（BGRA）读回 CPU 存进共享槽位，由配速线程按固定帧率取走。
//!   staging 纹理常驻复用（不再每帧新建），抓取间隔按显示器刷新率对齐到目标帧率，
//!   高刷屏上不会多搬运帧。
//! - 零拷贝引擎：画面只在 GPU 内拷贝到一张常驻纹理，由配速线程直接交给 Media Foundation 编码。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// 跨线程传递 D3D11 对象。设备已开启多线程保护，跨线程调用是安全的。
pub struct SendD3D<T>(pub T);
unsafe impl<T> Send for SendD3D<T> {}
unsafe impl<T> Sync for SendD3D<T> {}

#[derive(Default)]
pub struct FrameSlot {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// 零拷贝模式下的 GPU 帧：抓取线程持续把最新画面拷进 `texture`。
pub struct GpuFrame {
    pub device: SendD3D<ID3D11Device>,
    pub context: SendD3D<ID3D11DeviceContext>,
    pub texture: SendD3D<ID3D11Texture2D>,
}

pub struct FrameStore {
    pub slot: Mutex<FrameSlot>,
    pub gpu: Mutex<Option<GpuFrame>>,
    pub zero_copy: bool,
    pub first_frame: Condvar,
    pub captured: AtomicU64,
    pub stop: AtomicBool,
    pub closed: AtomicBool,
}

impl FrameStore {
    pub fn new(zero_copy: bool) -> Arc<Self> {
        Arc::new(Self {
            slot: Mutex::new(FrameSlot::default()),
            gpu: Mutex::new(None),
            zero_copy,
            first_frame: Condvar::new(),
            captured: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        })
    }

    /// 等待第一帧到达，返回 (宽, 高)。
    pub fn wait_first_frame(&self, timeout: Duration) -> Option<(u32, u32)> {
        let guard = self.slot.lock().unwrap();
        let (guard, _) = self
            .first_frame
            .wait_timeout_while(guard, timeout, |s| s.width == 0)
            .unwrap();
        if guard.width == 0 {
            None
        } else {
            Some((guard.width, guard.height))
        }
    }

    /// 把一张已映射的 BGRA 纹理（行距 `pitch`）写进槽位。
    fn push(&self, w: u32, h: u32, src: *const u8, pitch: usize) {
        let row_bytes = w as usize * 4;
        // 安全性：调用方保证 src 指向 h 行、每行至少 row_bytes 字节、行距 pitch 的已映射内存
        let row = |y: usize| unsafe { std::slice::from_raw_parts(src.add(y * pitch), row_bytes) };
        let mut slot = self.slot.lock().unwrap();
        if slot.width == 0 {
            // 第一帧：锁定输出尺寸
            slot.width = w;
            slot.height = h;
            slot.data = Vec::with_capacity(row_bytes * h as usize);
            for y in 0..h as usize {
                slot.data.extend_from_slice(row(y));
            }
            drop(slot);
            self.first_frame.notify_all();
            return;
        }
        let (dw, dh) = (slot.width as usize, slot.height as usize);
        let dst_stride = dw * 4;
        if dw == w as usize && dh == h as usize {
            for y in 0..dh {
                slot.data[y * dst_stride..(y + 1) * dst_stride].copy_from_slice(row(y));
            }
        } else {
            // 窗口尺寸变化：按已锁定尺寸裁剪 / 黑边填充
            slot.data.fill(0);
            let cw = dw.min(w as usize) * 4;
            let ch = dh.min(h as usize);
            for y in 0..ch {
                slot.data[y * dst_stride..y * dst_stride + cw].copy_from_slice(&row(y)[..cw]);
            }
        }
    }
}

pub struct Grabber {
    store: Arc<FrameStore>,
    /// 常驻 staging 纹理（读回模式）
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// 常驻 GPU 纹理（零拷贝模式）
    gpu_tex: Option<(ID3D11Texture2D, u32, u32)>,
}

impl Grabber {
    fn readback(&mut self, frame: &Frame, w: u32, h: u32) -> Result<(), BoxError> {
        let device = frame.device();
        let ctx = frame.device_context();
        let reuse = matches!(&self.staging, Some((_, sw, sh)) if *sw == w && *sh == h);
        if !reuse {
            let mut desc: D3D11_TEXTURE2D_DESC = *frame.desc();
            desc.MipLevels = 1;
            desc.ArraySize = 1;
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;
            let mut tex = None;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
            self.staging = Some((tex.ok_or("CreateTexture2D 返回空")?, w, h));
        }
        let (tex, _, _) = self.staging.as_ref().unwrap();
        unsafe {
            ctx.CopyResource(tex, frame.as_raw_texture());
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(tex, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            self.store.push(w, h, mapped.pData as *const u8, mapped.RowPitch as usize);
            ctx.Unmap(tex, 0);
        }
        Ok(())
    }

    fn copy_gpu(&mut self, frame: &Frame, w: u32, h: u32) -> Result<(), BoxError> {
        let device = frame.device();
        let ctx = frame.device_context();
        if self.gpu_tex.is_none() {
            // 首帧：按首帧尺寸建常驻纹理，之后尺寸变化只拷贝重叠区域
            let mut desc: D3D11_TEXTURE2D_DESC = *frame.desc();
            desc.MipLevels = 1;
            desc.ArraySize = 1;
            desc.Usage = D3D11_USAGE_DEFAULT;
            desc.BindFlags = (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32;
            desc.CPUAccessFlags = 0;
            desc.MiscFlags = 0;
            let mut tex = None;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
            let tex = tex.ok_or("CreateTexture2D 返回空")?;
            *self.store.gpu.lock().unwrap() = Some(GpuFrame {
                device: SendD3D(device.clone()),
                context: SendD3D(ctx.clone()),
                texture: SendD3D(tex.clone()),
            });
            self.gpu_tex = Some((tex, w, h));
            {
                let mut slot = self.store.slot.lock().unwrap();
                slot.width = w;
                slot.height = h;
            }
            self.store.first_frame.notify_all();
        }
        let (tex, tw, th) = self.gpu_tex.as_ref().unwrap();
        unsafe {
            if *tw == w && *th == h {
                ctx.CopyResource(tex, frame.as_raw_texture());
            } else {
                let b = D3D11_BOX { left: 0, top: 0, front: 0, right: w.min(*tw), bottom: h.min(*th), back: 1 };
                ctx.CopySubresourceRegion(tex, 0, 0, 0, 0, frame.as_raw_texture(), 0, Some(&b));
            }
        }
        Ok(())
    }
}

impl GraphicsCaptureApiHandler for Grabber {
    type Flags = Arc<FrameStore>;
    type Error = BoxError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        // 配速线程 / Media Foundation 会与抓取线程同时使用这个设备，必须开启多线程保护
        if let Ok(mt) = ctx.device_context.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = mt.SetMultithreadProtected(true);
            }
        }
        Ok(Self { store: ctx.flags, staging: None, gpu_tex: None })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.store.stop.load(Ordering::Relaxed) {
            control.stop();
            return Ok(());
        }
        let (w, h) = (frame.width(), frame.height());
        if self.store.zero_copy {
            self.copy_gpu(frame, w, h)?;
        } else {
            self.readback(frame, w, h)?;
        }
        self.store.captured.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.store.closed.store(true, Ordering::Relaxed);
        Ok(())
    }
}

#[derive(Clone)]
pub enum Target {
    Monitor(Monitor),
    Window(Window),
}

pub type Control = CaptureControl<Grabber, BoxError>;

pub fn start(target: Target, cursor: bool, fps: u32, store: Arc<FrameStore>) -> Result<Control, BoxError> {
    let cursor = if cursor {
        CursorCaptureSettings::WithCursor
    } else {
        CursorCaptureSettings::WithoutCursor
    };
    // WGC 的最小投递间隔会被向上取整到整数个刷新周期。取「不超过目标帧间隔的最大整数个
    // 刷新周期」再减半个周期作为阈值：高刷屏上抓取次数贴近目标帧率，又不会漏帧
    // （300Hz 屏录 60fps：每 5 个刷新周期投递一次，正好 60 次/秒）。
    let hz = match &target {
        Target::Monitor(m) => m.refresh_rate().ok(),
        Target::Window(w) => w.monitor().and_then(|m| m.refresh_rate().ok()),
    }
    .filter(|hz| *hz > 0)
    .unwrap_or(60) as f64;
    let periods = (hz / fps as f64 + 1e-6).floor().max(1.0);
    let min_interval = MinimumUpdateIntervalSettings::Custom(Duration::from_secs_f64((periods - 0.5) / hz));

    let control = match target {
        Target::Monitor(m) => Grabber::start_free_threaded(Settings::new(
            m,
            cursor,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            min_interval,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            store,
        ))?,
        Target::Window(w) => Grabber::start_free_threaded(Settings::new(
            w,
            cursor,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            min_interval,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            store,
        ))?,
    };
    Ok(control)
}

/// 在指定设备上建一张可被编码器读取的 BGRA 纹理（零拷贝引擎的帧池用）。
pub fn create_frame_texture(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D, BoxError> {
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    Ok(tex.ok_or("CreateTexture2D 返回空")?)
}
