//! DXGI Desktop Duplication：把一块屏幕区域直接从显卡抓下来，替代 GDI 的 BitBlt。
//!
//! 为什么快：GDI 抓屏要走 DWM 合成结果的回读（1080p 一次约 12ms，缩放时 17.8ms），
//! DXGI 直接拿 GPU 上的桌面纹理，只把我们需要的矩形 CopySubresourceRegion 到一张
//! staging 纹理再 Map 读回（1080p 约 4~7ms），而且不占 CPU 去合成。
//!
//! 只在一个输出（显示器）内部有效；跨屏、远程桌面、或别的程序已经占用复制会话时会失败，
//! 调用方自己回落到 GDI 那条路。

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIDevice,
    IDXGIOutput, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
};

/// 一次抓取的结果
pub enum Grabbed {
    /// 有新画面，已经写进目标缓冲
    New,
    /// 屏幕这一帧没变（AcquireNextFrame 超时），目标缓冲还是上一帧的内容
    Unchanged,
    /// 复制会话断了（分辨率变化 / 切换全屏 / 远程桌面介入），调用方该重建或回落 GDI
    Lost,
    /// 其它错误，这一帧当作没抓到
    Failed,
}

pub struct Duplicator {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplicator: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    /// 抓取区域在桌面上的位置（物理像素），画鼠标指针用
    origin: (i32, i32),
    /// 抓取区域在这个输出内部的位置：CopySubresourceRegion 用的是输出内坐标，不是桌面坐标
    local: (u32, u32),
    /// 实际能抓到的宽高（区域超出输出时会被裁掉）
    width: u32,
    height: u32,
}

impl Duplicator {
    /// 为一块桌面区域建复制会话；区域必须完整落在一个输出里，否则返回 None（走 GDI）
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Option<Self> {
        if w <= 0 || h <= 0 {
            return None;
        }
        unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .ok()?;
            let device = device?;
            let context = context?;

            let dxgi_device: IDXGIDevice = device.cast().ok()?;
            let adapter = dxgi_device.GetAdapter().ok()?;

            // 找包含这块区域的那个输出
            let mut output: Option<IDXGIOutput> = None;
            let mut index = 0u32;
            while let Ok(candidate) = adapter.EnumOutputs(index) {
                if let Ok(desc) = candidate.GetDesc() {
                    let r = desc.DesktopCoordinates;
                    if x >= r.left && y >= r.top && x + w <= r.right && y + h <= r.bottom {
                        output = Some(candidate);
                        break;
                    }
                }
                index += 1;
                if index > 8 {
                    break;
                }
            }
            let output = output?;

            let output1: IDXGIOutput1 = output.cast().ok()?;
            // DuplicateOutput1 可以指定想要的像素格式：这台机器是 HDR 桌面，原生格式是
            // R10G10B10A2（10bit），直接复制到 BGRA8 会静默失败、整帧全黑。
            // 用 DuplicateOutput1 让系统在 GPU 上帮我们转成 BGRA8（SDR 映射）。
            let duplicator = match output.cast::<IDXGIOutput5>() {
                Ok(output5) => output5
                    .DuplicateOutput1(&device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM])
                    .ok()?,
                Err(_) => output1.DuplicateOutput(&device).ok()?,
            };

            let dup_desc = duplicator.GetDesc();
            if dup_desc.ModeDesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                // 系统没给 BGRA8（老系统 / 特殊驱动）：别硬来，让调用方回落 GDI
                return None;
            }
            let full_w = dup_desc.ModeDesc.Width as i32;
            let full_h = dup_desc.ModeDesc.Height as i32;
            let output_rect = output.GetDesc().ok()?.DesktopCoordinates;
            let local_x = (x - output_rect.left).clamp(0, full_w.saturating_sub(1));
            let local_y = (y - output_rect.top).clamp(0, full_h.saturating_sub(1));
            let width = w.min(full_w - local_x).max(1) as u32;
            let height = h.min(full_h - local_y).max(1) as u32;

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
            device.CreateTexture2D(&desc, None, Some(&mut staging)).ok()?;

            Some(Self {
                _device: device,
                context,
                duplicator,
                staging: staging?,
                origin: (output_rect.left + local_x, output_rect.top + local_y),
                local: (local_x as u32, local_y as u32),
                width,
                height,
            })
        }
    }

    pub fn width(&self) -> i32 {
        self.width as i32
    }

    pub fn height(&self) -> i32 {
        self.height as i32
    }

    pub fn origin(&self) -> (i32, i32) {
        self.origin
    }

    /// 抓一帧，直接把 BGRA 行写进 `dst`（行距 `stride`）。
    /// 没有新画面时返回 `Unchanged`（`dst` 不动，调用方继续用上一帧）。
    pub fn grab(&mut self, dst: *mut u8, stride: usize) -> Grabbed {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            match self
                .duplicator
                .AcquireNextFrame(0, &mut info, &mut resource)
            {
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
                    let right = (left + self.width).min(src_desc.Width);
                    let bottom = (top + self.height).min(src_desc.Height);
                    let boxx = D3D11_BOX {
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
                        Some(&boxx),
                    );
                    let mut mapped = Default::default();
                    if self
                        .context
                        .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                        .is_ok()
                    {
                        let src_stride = mapped.RowPitch as usize;
                        let rows = (bottom - top) as usize;
                        let bytes = (self.width * 4) as usize;
                        let src = mapped.pData as *const u8;
                        for row in 0..rows {
                            std::ptr::copy_nonoverlapping(
                                src.add(row * src_stride),
                                dst.add(row * stride),
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
