//! DXGI Desktop Duplication：把一块屏幕区域直接从显卡抓下来，替代 GDI 的 BitBlt。
//!
//! 为什么快：GDI 抓屏要走 DWM 合成结果的回读（1080p 一次约 12ms，缩放时 17.8ms），
//! DXGI 直接拿 GPU 上的桌面纹理，只把需要的矩形 CopySubresourceRegion 到 staging
//! 纹理再 Map 读回（1080p 约 2~5ms），而且不占 CPU 去合成。
//!
//! 跨显示器也能用：选区横跨 N 块屏时，按每块屏各建一个复制会话，各自把自己那块
//! 拼进同一张 crop 位图（每块屏一个 device——Duplication 只能抓同适配器上的输出）。
//! 远程桌面、复制会话被别的程序（OBS 之类）占用、或拿不到 BGRA8 时返回 None，
//! 调用方回落到 GDI。

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    IDXGIFactory1, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
};

/// 一次抓取的结果（多块屏时按"有任何一个更新/断了"聚合）
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Grabbed {
    /// 至少一块屏有新画面，已经写进目标缓冲
    New,
    /// 所有屏都没变（AcquireNextFrame 超时），目标缓冲还是上一帧的内容
    Unchanged,
    /// 某个复制会话断了（分辨率变化 / 切换全屏 / 远程桌面介入），调用方该回落 GDI
    Lost,
    /// 所有输出都读失败，这一帧当作没抓到
    Failed,
}

/// 一块显示器上的一次复制会话
struct OutputSource {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplicator: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    /// 这块屏负责的区域在输出内的起点（CopySubresourceRegion 用的是输出内坐标）
    local: (u32, u32),
    /// 这块屏负责的宽高
    size: (u32, u32),
    /// 拼进整块 crop 时的位置
    offset: (i32, i32),

}

pub struct DesktopDuplicator {
    outputs: Vec<OutputSource>,
    /// 抓取区域在桌面上的位置（物理像素）
    origin: (i32, i32),
    width: i32,
    height: i32,
}

/// 两个矩形的交集，空的话返回 None
fn intersect(
    a: (i32, i32, i32, i32),
    b: (i32, i32, i32, i32),
) -> Option<(i32, i32, i32, i32)> {
    let left = a.0.max(b.0);
    let top = a.1.max(b.1);
    let right = (a.0 + a.2).min(b.0 + b.2);
    let bottom = (a.1 + a.3).min(b.1 + b.3);
    if right - left <= 0 || bottom - top <= 0 {
        None
    } else {
        Some((left, top, right - left, bottom - top))
    }
}

impl DesktopDuplicator {
    /// 为一块桌面区域（可以横跨多块屏）建复制会话；一块都建不起来就返回 None（走 GDI）
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Option<Self> {
        if w <= 0 || h <= 0 {
            return None;
        }
        let rect = (x, y, w, h);
        let mut outputs: Vec<OutputSource> = Vec::new();
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            let mut index = 0u32;
            while let Ok(adapter) = factory.EnumAdapters1(index) {
                // 同一块显卡的输出共用一个 device
                let mut device: Option<ID3D11Device> = None;
                let mut context: Option<ID3D11DeviceContext> = None;
                let mut output_index = 0u32;
                while let Ok(output) = adapter.EnumOutputs(output_index) {
                    if let Ok(desc) = output.GetDesc() {
                        let r = desc.DesktopCoordinates;
                        let out_rect = (r.left, r.top, r.right - r.left, r.bottom - r.top);
                        if let Some(part) = intersect(rect, out_rect) {
                            if device.is_none() {
                                let adapter_ref: windows::Win32::Graphics::Dxgi::IDXGIAdapter =
                                    adapter.cast().ok()?;
                                let mut dev: Option<ID3D11Device> = None;
                                let mut ctx: Option<ID3D11DeviceContext> = None;
                                D3D11CreateDevice(
                                    Some(&adapter_ref),
                                    D3D_DRIVER_TYPE_UNKNOWN,
                                    Default::default(),
                                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                                    None,
                                    D3D11_SDK_VERSION,
                                    Some(&mut dev),
                                    None,
                                    Some(&mut ctx),
                                )
                                .ok()?;
                                device = Some(dev?);
                                context = Some(ctx?);
                            }
                            let device_ref = device.as_ref()?;
                            let context_ref = context.as_ref()?;
                            if let Ok(source) = create_source(
                                &output, device_ref, context_ref, rect, part, (r.left, r.top),
                                (out_rect.2, out_rect.3),
                            ) {
                                outputs.push(source);
                            } else {
                                // 有一块屏建不起来（被占用 / 非 BGRA8 / HDR 未映射）：
                                // 整个退回 GDI，免得录出半黑画面
                                return None;
                            }
                        }
                    }
                    output_index += 1;
                    if output_index > 8 {
                        break;
                    }
                }
                index += 1;
                if index > 8 {
                    break;
                }
            }
        }
        if outputs.is_empty() {
            return None;
        }
        Some(Self {
            outputs,
            origin: (x, y),
            width: w,
            height: h,
        })
    }

    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    pub fn origin(&self) -> (i32, i32) {
        self.origin
    }

    /// 用了几个输出（1 = 单屏，>1 = 跨屏拼接）
    pub fn output_count(&self) -> usize {
        self.outputs.len()
    }

    /// 抓一帧，直接把 BGRA 行写进 `dst`（行距 `stride`，尺寸 = width×height）。
    pub fn grab(&mut self, dst: *mut u8, stride: usize) -> Grabbed {
        let mut new = 0;
        let mut failed = 0;
        for source in &mut self.outputs {
            match source.grab(dst, stride) {
                Grabbed::New => new += 1,
                Grabbed::Lost => return Grabbed::Lost,
                Grabbed::Failed => failed += 1,
                Grabbed::Unchanged => {}
            }
        }
        if new > 0 {
            Grabbed::New
        } else if failed == self.outputs.len() {
            Grabbed::Failed
        } else {
            Grabbed::Unchanged
        }
    }
}

/// 给一块输出建复制会话 + 它的 staging 纹理
unsafe fn create_source(
    output: &windows::Win32::Graphics::Dxgi::IDXGIOutput,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    rect: (i32, i32, i32, i32),
    part: (i32, i32, i32, i32),
    output_origin: (i32, i32),
    _output_size: (i32, i32),
) -> windows::core::Result<OutputSource> {
    // DuplicateOutput1 可以指定想要的像素格式：HDR 桌面原生是 R10G10B10A2（10bit），
    // 直接复制到 BGRA8 会静默失败、整帧全黑。让系统在 GPU 上映射成 SDR 的 BGRA8。
    let duplicator = match output.cast::<IDXGIOutput5>() {
        Ok(output5) => output5.DuplicateOutput1(device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM])?,
        Err(_) => output.cast::<IDXGIOutput1>()?.DuplicateOutput(device)?,
    };
    let dup_desc = duplicator.GetDesc();
    if dup_desc.ModeDesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
        // 拿不到 BGRA8（老系统 / 特殊驱动）：别硬来，让调用方回落 GDI
        return Err(windows::core::Error::from(windows::Win32::Foundation::E_FAIL));
    }

    let width = part.2 as u32;
    let height = part.3 as u32;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    device.CreateTexture2D(&desc, None, Some(&mut staging))?;

    Ok(OutputSource {
        _device: device.clone(),
        context: context.clone(),
        duplicator,
        staging: staging.unwrap(),
        local: (
            (part.0 - output_origin.0).max(0) as u32,
            (part.1 - output_origin.1).max(0) as u32,
        ),
        size: (width, height),
        offset: (part.0 - rect.0, part.1 - rect.1),

    })
}

impl OutputSource {
    fn grab(&mut self, dst: *mut u8, stride: usize) -> Grabbed {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            match self.duplicator.AcquireNextFrame(0, &mut info, &mut resource) {
                Ok(()) => {}
                Err(err) if err.code() == DXGI_ERROR_WAIT_TIMEOUT => return Grabbed::Unchanged,
                Err(err) if err.code() == DXGI_ERROR_ACCESS_LOST => return Grabbed::Lost,
                Err(_) => return Grabbed::Failed,
            }
            let mut result = Grabbed::Failed;
            if let Some(resource) = resource {
                if let Ok(texture) = resource.cast::<ID3D11Texture2D>() {
                    let mut src_desc = D3D11_TEXTURE2D_DESC::default();
                    texture.GetDesc(&mut src_desc);
                    let left = self.local.0;
                    let top = self.local.1;
                    let right = (left + self.size.0).min(src_desc.Width);
                    let bottom = (top + self.size.1).min(src_desc.Height);
                    let region = D3D11_BOX {
                        left,
                        top,
                        front: 0,
                        right,
                        bottom,
                        back: 1,
                    };
                    self.context.CopySubresourceRegion(
                        &self.staging,
                        0,
                        0,
                        0,
                        0,
                        &texture,
                        0,
                        Some(&region),
                    );
                    let mut mapped = Default::default();
                    if self
                        .context
                        .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                        .is_ok()
                    {
                        let src_stride = mapped.RowPitch as usize;
                        let rows = (bottom - top) as usize;
                        let bytes = (self.size.0 * 4) as usize;
                        let src = mapped.pData as *const u8;
                        let base = dst.add(
                            self.offset.1.max(0) as usize * stride
                                + self.offset.0.max(0) as usize * 4,
                        );
                        for row in 0..rows {
                            std::ptr::copy_nonoverlapping(
                                src.add(row * src_stride),
                                base.add(row * stride),
                                bytes,
                            );
                        }
                        self.context.Unmap(&self.staging, 0);
                        result = Grabbed::New;
                    }
                }
            }
            let _ = self.duplicator.ReleaseFrame();
            result
        }
    }
}
