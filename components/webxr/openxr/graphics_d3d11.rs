/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::{mem, ptr};
use std::sync::atomic::{AtomicUsize, Ordering};

use euclid::{Size2D, UnknownUnit};
use log::warn;
use openxr::d3d::{D3D11, Requirements, SessionCreateInfoD3D11};
use openxr::{
    ExtensionSet, FormFactor, FrameStream, FrameWaiter, Graphics, Instance, Session, SystemId,
};
use surfman::{
    Adapter as SurfmanAdapter, Connection as SurfmanConnection, Context as SurfmanContext,
    Device as SurfmanDevice, Error as SurfmanError, SurfaceTexture,
};
use webxr_api::Error;
use winapi::Interface;
use winapi::shared::winerror::{DXGI_ERROR_NOT_FOUND, S_OK};
use winapi::shared::{dxgi, dxgiformat};
use winapi::um::d3d11::{ID3D11Device, ID3D11Texture2D};
use wio::com::ComPtr;

use crate::openxr::graphics::{GraphicsProvider, GraphicsProviderMethods};

pub type Backend = D3D11;

/// `xrCreateSession` に渡した D3D11 デバイスの生ポインタを保持する。
///
/// XR スワップチェーン画像はこのデバイスの所有物であり、`xr_copy` が使う
/// デバイスとの同一判定の基準とする。`native_device()` は参照を1つ足して返すため、
/// ここに格納した分は解放せず、セッション存続中は有効に保つ。
/// 不変条件: 0 は未設定であり、設定後は不変である。
static SESSION_D3D_DEVICE: AtomicUsize = AtomicUsize::new(0);

/// セッション作成時に XR ランタイムへ渡したデバイスを返す。未作成時は null である。
pub(super) fn session_device_ptr() -> *mut ID3D11Device {
    SESSION_D3D_DEVICE.load(Ordering::SeqCst) as *mut ID3D11Device
}

impl GraphicsProviderMethods<D3D11> for GraphicsProvider {
    fn enable_graphics_extensions(exts: &mut ExtensionSet) {
        exts.khr_d3d11_enable = true;
    }

    fn pick_format(formats: &[u32]) -> u32 {
        // TODO: extract the format from surfman's device and pick a matching
        // valid format based on that. For now, assume that eglChooseConfig will
        // gravitate to B8G8R8A8.
        warn!("Available formats: {:?}", formats);
        for format in formats {
            match *format {
                // ADR: ANGLE共有pbuffer（TEXTURE_RGBA由来）はUNORMであり、
                // GPUコピー経路のCopyResourceは形式一致を要求するためUNORMを優先する。
                // SRGBが選ばれると転送元と形式が合わず層の初期化に失敗する。
                dxgiformat::DXGI_FORMAT_B8G8R8A8_UNORM => return *format,
                dxgiformat::DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => return *format,
                // dxgiformat::DXGI_FORMAT_R8G8B8A8_UNORM => return *format,
                f => {
                    warn!("Backend requested unsupported format {:?}", f);
                },
            }
        }

        panic!("No formats supported amongst {:?}", formats);
    }

    fn create_session(
        instance: &Instance,
        system: SystemId,
    ) -> Result<(Session<D3D11>, FrameWaiter, FrameStream<D3D11>), Error> {
        // FIXME: We should ensure that the OpenXR runtime's texture will be shareable with
        // surfman's surfaces by validating the requirements of the XR runtime and ensuring that
        // the WebGL context can use the same device when `makeXRCompatible` is called.
        let adapter = create_surfman_adapter(instance).ok_or(Error::NoMatchingDevice)?;
        let connection = SurfmanConnection::new().map_err(|_| Error::NoMatchingDevice)?;
        let device = connection
            .create_device(&adapter)
            .map_err(|_| Error::NoMatchingDevice)?;
        let native_device = device.native_device();
        let d3d_device = native_device.d3d11_device;
        // デバイス同一判定の基準として保持する。native_device() の +1 参照は放さず、
        // セッション存続中の有効性を保証する（終了時に回収される一時的なリーク）。
        SESSION_D3D_DEVICE.store(d3d_device as usize, Ordering::SeqCst);

        unsafe {
            instance
                .create_session::<D3D11>(
                    system,
                    &SessionCreateInfoD3D11 {
                        device: d3d_device as *mut _,
                    },
                )
                .map_err(|e| Error::BackendSpecific(format!("Instance::create_session {:?}", e)))
        }
    }

    fn surface_texture_from_swapchain_texture(
        image: <D3D11 as Graphics>::SwapchainImage,
        device: &SurfmanDevice,
        context: &mut SurfmanContext,
        size: &Size2D<i32, UnknownUnit>,
    ) -> Result<SurfaceTexture, SurfmanError> {
        unsafe {
            let image = ComPtr::from_raw(image as *mut ID3D11Texture2D);
            image.AddRef();
            device.create_surface_texture_from_texture(context, size, image)
        }
    }
}

fn get_matching_adapter(
    requirements: &Requirements,
) -> Result<ComPtr<dxgi::IDXGIAdapter1>, String> {
    unsafe {
        let mut factory_ptr: *mut dxgi::IDXGIFactory1 = ptr::null_mut();
        let result = dxgi::CreateDXGIFactory1(
            &dxgi::IDXGIFactory1::uuidof(),
            &mut factory_ptr as *mut _ as *mut _,
        );
        assert_eq!(result, S_OK);
        let factory = ComPtr::from_raw(factory_ptr);

        let index = 0;
        loop {
            let mut adapter_ptr = ptr::null_mut();
            let result = factory.EnumAdapters1(index, &mut adapter_ptr);
            if result == DXGI_ERROR_NOT_FOUND {
                return Err("No matching adapter".to_owned());
            }
            assert_eq!(result, S_OK);
            let adapter = ComPtr::from_raw(adapter_ptr);
            let mut adapter_desc = mem::zeroed();
            let result = adapter.GetDesc1(&mut adapter_desc);
            assert_eq!(result, S_OK);
            let adapter_luid = &adapter_desc.AdapterLuid;
            if adapter_luid.LowPart == requirements.adapter_luid.LowPart &&
                adapter_luid.HighPart == requirements.adapter_luid.HighPart
            {
                return Ok(adapter);
            }
        }
    }
}

fn create_surfman_adapter(instance: &Instance) -> Option<SurfmanAdapter> {
    let system = instance.system(FormFactor::HEAD_MOUNTED_DISPLAY).ok()?;

    let requirements = D3D11::requirements(instance, system).ok()?;
    let adapter = get_matching_adapter(&requirements).ok()?;
    Some(SurfmanAdapter::from_dxgi_adapter(adapter.up()))
}
