//! Direct3D 11 device creation and GPU → CPU readback through a staging texture.

use ::windows::Win32::Foundation::HMODULE;
use ::windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_10_0,
    D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use ::windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use ::windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_SAMPLE_DESC,
};
use ::windows::Win32::Graphics::Dxgi::IDXGIAdapter1;
use ::windows::core::Interface;

use super::os_err;
use crate::windows_pure::clamp_rect;
use crate::{BgraImage, CaptureError, Rect, Result};

/// Creates a BGRA-capable D3D11 device: on `adapter` when given (Desktop Duplication must
/// use the adapter that owns the output), otherwise on the default hardware adapter with a
/// WARP fallback.
pub(crate) fn create_device(adapter: Option<&IDXGIAdapter1>) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
    let try_create = |driver: D3D_DRIVER_TYPE| -> ::windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
        let mut device = None;
        let mut ctx = None;
        // SAFETY: out-pointers live for the call; the adapter (if any) is a valid COM object.
        unsafe {
            let a = adapter.map(|a| a.cast::<::windows::Win32::Graphics::Dxgi::IDXGIAdapter>()).transpose()?;
            D3D11CreateDevice(
                a.as_ref(),
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )?;
        }
        match (device, ctx) {
            (Some(d), Some(c)) => Ok((d, c)),
            _ => Err(::windows::core::Error::from(::windows::Win32::Foundation::E_POINTER)),
        }
    };
    if adapter.is_some() {
        return try_create(D3D_DRIVER_TYPE_UNKNOWN).map_err(|e| os_err("D3D11CreateDevice(adapter)", e));
    }
    try_create(D3D_DRIVER_TYPE_HARDWARE)
        .or_else(|_| try_create(D3D_DRIVER_TYPE_WARP))
        .map_err(|e| os_err("D3D11CreateDevice", e))
}

fn is_bgra(f: DXGI_FORMAT) -> bool {
    f == DXGI_FORMAT_B8G8R8A8_UNORM || f == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
}

/// A reusable staging texture for reading BGRA textures back to the CPU.
#[derive(Default)]
pub(crate) struct Readback {
    staging: Option<(ID3D11Texture2D, u32, u32, DXGI_FORMAT)>,
}

impl Readback {
    fn staging_for(&mut self, device: &ID3D11Device, desc: &D3D11_TEXTURE2D_DESC) -> Result<ID3D11Texture2D> {
        if let Some((t, w, h, f)) = &self.staging
            && (*w, *h, *f) == (desc.Width, desc.Height, desc.Format)
        {
            return Ok(t.clone());
        }
        let sd = D3D11_TEXTURE2D_DESC {
            Width: desc.Width,
            Height: desc.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut tex = None;
        // SAFETY: valid descriptor and out-pointer.
        unsafe { device.CreateTexture2D(&sd, None, Some(&mut tex)) }.map_err(|e| os_err("CreateTexture2D(staging)", e))?;
        let tex = tex.ok_or_else(|| CaptureError::Os("CreateTexture2D returned no texture".into()))?;
        self.staging = Some((tex.clone(), desc.Width, desc.Height, desc.Format));
        Ok(tex)
    }

    /// Copies the top-left `crop` (or the whole texture) of `src` into `dst`.
    ///
    /// When `dst` already has the target size and `rects` is `Some`, only those regions are
    /// copied (the rest of `dst` is assumed current); otherwise `dst` is replaced entirely.
    pub(crate) fn read(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        crop: Option<(u32, u32)>,
        dst: &mut Option<BgraImage>,
        rects: Option<&[Rect]>,
    ) -> Result<()> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: out-pointer of the right type.
        unsafe { src.GetDesc(&mut desc) };
        if !is_bgra(desc.Format) {
            return Err(CaptureError::InvalidFrame(format!("unexpected texture format {:?}", desc.Format)));
        }
        let (w, h) = crop.map_or((desc.Width, desc.Height), |(cw, ch)| (cw.min(desc.Width), ch.min(desc.Height)));
        if w == 0 || h == 0 {
            return Err(CaptureError::InvalidFrame("empty texture".into()));
        }
        let staging = self.staging_for(device, &desc)?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: both textures belong to `device`, have identical size/format; Map blocks
        // until the copy completed and the mapping is released by Unmap below.
        unsafe {
            ctx.CopyResource(&staging, src);
            ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).map_err(|e| os_err("Map(staging)", e))?;
        }
        let pitch = mapped.RowPitch as usize;
        let row_bytes = w as usize * 4;
        // SAFETY: the mapping covers `RowPitch × Height` bytes and stays valid until Unmap.
        let src_bytes = unsafe {
            std::slice::from_raw_parts(mapped.pData as *const u8, pitch * (desc.Height as usize - 1) + desc.Width as usize * 4)
        };
        let partial = matches!(dst, Some(img) if img.width == w && img.height == h) && rects.is_some();
        if partial {
            let img = dst.as_mut().expect("checked");
            for r in rects.unwrap_or_default().iter().filter_map(|r| clamp_rect(*r, w, h)) {
                let (x0, x1) = (r.x as usize * 4, (r.x as usize + r.width as usize) * 4);
                for y in r.y as usize..r.y as usize + r.height as usize {
                    img.data[y * img.stride + x0..y * img.stride + x1]
                        .copy_from_slice(&src_bytes[y * pitch + x0..y * pitch + x1]);
                }
            }
        } else {
            let mut img = BgraImage::new(w, h);
            for y in 0..h as usize {
                img.data[y * row_bytes..(y + 1) * row_bytes].copy_from_slice(&src_bytes[y * pitch..y * pitch + row_bytes]);
            }
            *dst = Some(img);
        }
        // SAFETY: matches the successful Map above.
        unsafe { ctx.Unmap(&staging, 0) };
        Ok(())
    }
}
