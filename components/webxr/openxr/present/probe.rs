/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL-2.0/. */

//! Servo(ANGLE/GL)の描画結果を別D3D11デバイスへ渡せるかを判定するための能力計測。
//!
//! 画素の読み戻しはしない。`OpenXrLayerManager::create_layer`が最初に呼ばれた1回だけ
//! 小さなpbuffer共有ハンドルを作り、XRアダプタ上のD3D11デバイスで開けるかを記録する。
//! どこで失敗してもpanicさせず、各段階の成否を1行JSONで残して先へ進む。
//! 計測後は削除予定のため、このモジュールに集約する。

use std::sync::atomic::{AtomicBool, Ordering};

use surfman::{SurfaceAccess, SurfaceType};

use super::super::Backend;
use crate::SurfmanGL;

static DONE: AtomicBool = AtomicBool::new(false);

/// 移動前のログtarget。`mod xr_probe` にあった呼び出しの観測互換を保つ。
/// W3で診断を整理するまで変えない。
const OLD_LOG_TARGET: &str = "servo_webxr::openxr::xr_probe";

/// `create_layer`の先頭から呼ぶ。2回目以降は何もしない。
pub(super) fn probe_once(
    session: &std::sync::Arc<openxr::Session<Backend>>,
    contexts: &mut dyn webxr_api::GLContexts<SurfmanGL>,
    context_id: webxr_api::ContextId,
) {
    if DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    let line = run(session, contexts, context_id);
    // 移動前のtargetを保つ（W3で整理）。
    log::info!(target: OLD_LOG_TARGET, "[xr-probe] {line}");
    // 実行担当の指示に従い、`XR_PROBE_LOG`があればそこにも追記する。
    if let Ok(path) = std::env::var("XR_PROBE_LOG") {
        if !path.is_empty() {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                use std::io::Write as _;
                let _ = writeln!(file, "{line}");
            }
        }
    }
}

fn run(
    session: &std::sync::Arc<openxr::Session<Backend>>,
    contexts: &mut dyn webxr_api::GLContexts<SurfmanGL>,
    context_id: webxr_api::ContextId,
) -> String {
    let mut angle = serde_json::Map::new();
    let mut surface = serde_json::Map::new();
    let mut xr = serde_json::Map::new();

    // 1. WebGL側のdevice/contextを取得し、D3D11アダプタ情報とGL文字列を記録する。
    let device = contexts.device(context_id);
    angle.insert(
        "has_device".into(),
        serde_json::Value::Bool(device.is_some()),
    );
    let mut angle_luid: Option<(u32, i32)> = None;
    if let Some(device) = device.as_ref() {
        use winapi::Interface as _;
        let native = device.native_device();
        if native.d3d11_device.is_null() {
            angle.insert(
                "d3d11_device_null".into(),
                serde_json::Value::Bool(true),
            );
        } else {
            // `native_device()`は参照を1つ足したまま返すため、ComPtrで包んで解放する。
            let raw = native.d3d11_device as *mut winapi::um::d3d11::ID3D11Device;
            let raw_device = unsafe { wio::com::ComPtr::from_raw(raw) };
            // 機能レベルはID3D11Deviceから直接取れる。
            let feature_level = unsafe { raw_device.GetFeatureLevel() };
            angle.insert(
                "feature_level".into(),
                serde_json::json!(format!("{feature_level:#x}")),
            );
            // アダプタLUIDはDXGI経由で取得する。
            // winapi 0.3のID3D11DeviceにはGetCreationParametersが無いための代替経路である。
            unsafe {
                let mut dxgi_device: *mut winapi::shared::dxgi::IDXGIDevice =
                    std::ptr::null_mut();
                let qi_hr = raw_device.QueryInterface(
                    &winapi::shared::dxgi::IDXGIDevice::uuidof(),
                    &mut dxgi_device as *mut *mut winapi::shared::dxgi::IDXGIDevice
                        as *mut *mut std::ffi::c_void,
                );
                angle.insert(
                    "qi_dxgi_device_hr".into(),
                    serde_json::json!(format!("{qi_hr:#x}")),
                );
                if qi_hr == winapi::shared::winerror::S_OK && !dxgi_device.is_null() {
                    let dxgi_device = wio::com::ComPtr::from_raw(dxgi_device);
                    let mut adapter: *mut winapi::shared::dxgi::IDXGIAdapter =
                        std::ptr::null_mut();
                    let adapter_hr = dxgi_device.GetAdapter(&mut adapter);
                    angle.insert(
                        "get_adapter_hr".into(),
                        serde_json::json!(format!("{adapter_hr:#x}")),
                    );
                    if adapter_hr == winapi::shared::winerror::S_OK && !adapter.is_null() {
                        let adapter = wio::com::ComPtr::from_raw(adapter);
                        let mut adapter1: *mut winapi::shared::dxgi::IDXGIAdapter1 =
                            std::ptr::null_mut();
                        let qi1_hr = adapter.QueryInterface(
                            &winapi::shared::dxgi::IDXGIAdapter1::uuidof(),
                            &mut adapter1 as *mut *mut winapi::shared::dxgi::IDXGIAdapter1
                                as *mut *mut std::ffi::c_void,
                        );
                        angle.insert(
                            "qi_adapter1_hr".into(),
                            serde_json::json!(format!("{qi1_hr:#x}")),
                        );
                        if qi1_hr == winapi::shared::winerror::S_OK && !adapter1.is_null() {
                            let adapter1 = wio::com::ComPtr::from_raw(adapter1);
                            let mut desc: winapi::shared::dxgi::DXGI_ADAPTER_DESC1 =
                                std::mem::zeroed();
                            let desc_hr = adapter1.GetDesc1(&mut desc);
                            angle.insert(
                                "adapter_desc_hr".into(),
                                serde_json::json!(format!("{desc_hr:#x}")),
                            );
                            if desc_hr == winapi::shared::winerror::S_OK {
                                angle.insert(
                                    "adapter_luid_low".into(),
                                    serde_json::json!(desc.AdapterLuid.LowPart),
                                );
                                angle.insert(
                                    "adapter_luid_high".into(),
                                    serde_json::json!(desc.AdapterLuid.HighPart),
                                );
                                angle_luid = Some((
                                    desc.AdapterLuid.LowPart,
                                    desc.AdapterLuid.HighPart,
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(gl) = contexts.bindings(context_id) {
        use glow::HasContext as _;
        // SAFETY: RENDERER/VENDOR/VERSIONは有効な列挙子であり、
        // 取得した文字列はこのブロック内でJSONへ複写するだけである。
        let (renderer, vendor, version) = unsafe {
            (
                gl.get_parameter_string(glow::RENDERER),
                gl.get_parameter_string(glow::VENDOR),
                gl.get_parameter_string(glow::VERSION),
            )
        };
        angle.insert("gl_renderer".into(), serde_json::json!(renderer));
        angle.insert("gl_vendor".into(), serde_json::json!(vendor));
        angle.insert("gl_version".into(), serde_json::json!(version));
    } else {
        angle.insert(
            "has_bindings".into(),
            serde_json::Value::Bool(false),
        );
    }
    angle.insert(
        "libegl_path".into(),
        serde_json::json!(module_path("libEGL.dll")),
    );
    angle.insert(
        "libglesv2_path".into(),
        serde_json::json!(module_path("libGLESv2.dll")),
    );

    // 2. 小さな共有pbufferを作り、ハンドル取得とXR側での開放を試す。
    // 画素は読まない。surfaceは最後に明示的に破棄する。
    match contexts.context(context_id) {
        Some(context) => {
            surface.insert(
                "has_context".into(),
                serde_json::Value::Bool(true),
            );
            match device.as_ref() {
                Some(device) => {
                    let size = euclid::Size2D::new(64, 64);
                    match device.create_surface(
                        &*context,
                        SurfaceAccess::GPUOnly,
                        SurfaceType::Generic { size },
                    ) {
                        Ok(mut probe_surface) => {
                            surface.insert(
                                "created".into(),
                                serde_json::Value::Bool(true),
                            );
                            let handle = probe_surface.share_handle();
                            surface.insert(
                                "share_handle".into(),
                                match handle {
                                    Some(handle) => serde_json::json!(format!(
                                        "{:#x}",
                                        handle as usize
                                    )),
                                    None => serde_json::Value::Null,
                                },
                            );
                            probe_open(session, handle, &mut xr, angle_luid);
                            match device.destroy_surface(&mut *context, &mut probe_surface) {
                                Ok(()) => {
                                    surface.insert(
                                        "destroyed".into(),
                                        serde_json::Value::Bool(true),
                                    );
                                },
                                Err(error) => {
                                    surface.insert(
                                        "destroyed".into(),
                                        serde_json::Value::Bool(false),
                                    );
                                    surface.insert(
                                        "destroy_error".into(),
                                        serde_json::json!(format!("{error:?}")),
                                    );
                                    // 破棄失敗のままDropするとpanicするため、リークを選ぶ。
                                    // ADR: 計測用の一時surfaceであり、終了時に回収される。
                                    std::mem::forget(probe_surface);
                                },
                            }
                        },
                        Err(error) => {
                            surface.insert(
                                "created".into(),
                                serde_json::Value::Bool(false),
                            );
                            surface.insert(
                                "error".into(),
                                serde_json::json!(format!("{error:?}")),
                            );
                            probe_open(session, None, &mut xr, angle_luid);
                        },
                    }
                },
                None => {
                    probe_open(session, None, &mut xr, angle_luid);
                },
            }
        },
        None => {
            surface.insert(
                "has_context".into(),
                serde_json::Value::Bool(false),
            );
            probe_open(session, None, &mut xr, angle_luid);
        },
    }

    let root = serde_json::json!({
        "event": "xr-probe",
        "angle": angle,
        "surface": surface,
        "xr": xr,
    });
    serde_json::to_string(&root).unwrap_or_else(|_| "{\"event\":\"xr-probe-encode-error\"}".into())
}

/// XRランタイムの要求LUIDを取得し、同一アダプタ上のD3D11デバイスで共有ハンドルを開く。
///
/// 注: `graphics_d3d11.rs`が作ったデバイス本体ではなく、同一アダプタ上に作り直した
/// 別デバイスである。デバイス間共有の可否を見る目的は同じである。
fn probe_open(
    session: &std::sync::Arc<openxr::Session<Backend>>,
    handle: Option<*mut std::ffi::c_void>,
    xr: &mut serde_json::Map<String, serde_json::Value>,
    angle_luid: Option<(u32, i32)>,
) {
    use openxr::{d3d::D3D11, FormFactor, Graphics};
    use winapi::Interface as _;

    let system = match session
        .instance()
        .system(FormFactor::HEAD_MOUNTED_DISPLAY)
    {
        Ok(system) => system,
        Err(error) => {
            xr.insert(
                "system_error".into(),
                serde_json::json!(format!("{error:?}")),
            );
            return;
        },
    };
    let requirements = match D3D11::requirements(session.instance(), system) {
        Ok(requirements) => requirements,
        Err(error) => {
            xr.insert(
                "requirements_error".into(),
                serde_json::json!(format!("{error:?}")),
            );
            return;
        },
    };
    xr.insert(
        "req_luid_low".into(),
        serde_json::json!(requirements.adapter_luid.LowPart),
    );
    xr.insert(
        "req_luid_high".into(),
        serde_json::json!(requirements.adapter_luid.HighPart),
    );
    xr.insert(
        "req_min_feature_level".into(),
        serde_json::json!(format!("{:#x}", requirements.min_feature_level as u32)),
    );
    xr.insert(
        "luid_match".into(),
        serde_json::Value::Bool(
            angle_luid ==
                Some((
                    requirements.adapter_luid.LowPart as u32,
                    requirements.adapter_luid.HighPart as i32,
                )),
        ),
    );

    let xr_device = match create_device_on_adapter(
        requirements.adapter_luid.LowPart as u32,
        requirements.adapter_luid.HighPart as i32,
    ) {
        Ok(device) => device,
        Err(error) => {
            xr.insert("xr_device_error".into(), serde_json::json!(error));
            return;
        },
    };
    xr.insert(
        "xr_device_created".into(),
        serde_json::Value::Bool(true),
    );

    let handle = match handle {
        Some(handle) => handle,
        None => {
            xr.insert(
                "open_skipped".into(),
                serde_json::json!("no share handle"),
            );
            return;
        },
    };
    unsafe {
        let mut opened: *mut std::ffi::c_void = std::ptr::null_mut();
        let open_hr = xr_device.OpenSharedResource(
            handle,
            &winapi::um::d3d11::ID3D11Texture2D::uuidof(),
            &mut opened,
        );
        xr.insert(
            "open_hr".into(),
            serde_json::json!(format!("{open_hr:#x}")),
        );
        if open_hr != winapi::shared::winerror::S_OK || opened.is_null() {
            xr.insert("opened".into(), serde_json::Value::Bool(false));
            return;
        }
        xr.insert("opened".into(), serde_json::Value::Bool(true));
        let texture =
            wio::com::ComPtr::from_raw(opened as *mut winapi::um::d3d11::ID3D11Texture2D);
        // IDXGISurface::GetDescで寸法と形式だけを見る。画素は読まない。
        let mut surface_ptr: *mut winapi::shared::dxgi::IDXGISurface =
            std::ptr::null_mut();
        let query_hr = texture.QueryInterface(
            &winapi::shared::dxgi::IDXGISurface::uuidof(),
            &mut surface_ptr as *mut *mut winapi::shared::dxgi::IDXGISurface
                as *mut *mut std::ffi::c_void,
        );
        xr.insert(
            "qi_surface_hr".into(),
            serde_json::json!(format!("{query_hr:#x}")),
        );
        if query_hr != winapi::shared::winerror::S_OK || surface_ptr.is_null() {
            return;
        }
        let surface_object = wio::com::ComPtr::from_raw(surface_ptr);
        let mut desc: winapi::shared::dxgi::DXGI_SURFACE_DESC = std::mem::zeroed();
        let desc_hr = surface_object.GetDesc(&mut desc);
        xr.insert(
            "desc_hr".into(),
            serde_json::json!(format!("{desc_hr:#x}")),
        );
        if desc_hr == winapi::shared::winerror::S_OK {
            xr.insert("tex_width".into(), serde_json::json!(desc.Width));
            xr.insert("tex_height".into(), serde_json::json!(desc.Height));
            xr.insert("tex_format".into(), serde_json::json!(desc.Format));
            xr.insert(
                "tex_sample_count".into(),
                serde_json::json!(desc.SampleDesc.Count),
            );
        }
    }
}

/// XRランタイムの要求LUIDに一致するアダプタでD3D11デバイスを作る。
fn create_device_on_adapter(
    luid_low: u32,
    luid_high: i32,
) -> Result<wio::com::ComPtr<winapi::um::d3d11::ID3D11Device>, String> {
    use winapi::Interface as _;
    use winapi::shared::winerror::S_OK;
    unsafe {
        let mut factory_ptr: *mut winapi::shared::dxgi::IDXGIFactory1 =
            std::ptr::null_mut();
        let factory_hr = winapi::shared::dxgi::CreateDXGIFactory1(
            &winapi::shared::dxgi::IDXGIFactory1::uuidof(),
            &mut factory_ptr as *mut *mut winapi::shared::dxgi::IDXGIFactory1
                as *mut *mut std::ffi::c_void,
        );
        if factory_hr != S_OK || factory_ptr.is_null() {
            return Err(format!("CreateDXGIFactory1 failed: {factory_hr:#x}"));
        }
        let factory = wio::com::ComPtr::from_raw(factory_ptr);

        let mut index = 0;
        let adapter = loop {
            let mut adapter_ptr: *mut winapi::shared::dxgi::IDXGIAdapter1 =
                std::ptr::null_mut();
            let enum_hr = factory.EnumAdapters1(index, &mut adapter_ptr);
            if enum_hr == winapi::shared::winerror::DXGI_ERROR_NOT_FOUND {
                return Err("no DXGI adapter matches runtime LUID".to_string());
            }
            if enum_hr != S_OK || adapter_ptr.is_null() {
                return Err(format!("EnumAdapters1({index}) failed: {enum_hr:#x}"));
            }
            let candidate = wio::com::ComPtr::from_raw(adapter_ptr);
            let mut desc: winapi::shared::dxgi::DXGI_ADAPTER_DESC1 = std::mem::zeroed();
            let desc_hr = candidate.GetDesc1(&mut desc);
            if desc_hr == S_OK &&
                desc.AdapterLuid.LowPart == luid_low &&
                desc.AdapterLuid.HighPart == luid_high
            {
                break candidate;
            }
            index += 1;
        };

        let mut device: *mut winapi::um::d3d11::ID3D11Device = std::ptr::null_mut();
        let mut feature_level = 0;
        // 機能レベル配列を渡さず既定の降順選択に任せる。
        let device_hr = winapi::um::d3d11::D3D11CreateDevice(
            adapter.as_raw() as *mut winapi::shared::dxgi::IDXGIAdapter,
            winapi::um::d3dcommon::D3D_DRIVER_TYPE_UNKNOWN,
            std::ptr::null_mut(),
            winapi::um::d3d11::D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            std::ptr::null(),
            0,
            winapi::um::d3d11::D3D11_SDK_VERSION,
            &mut device,
            &mut feature_level,
            std::ptr::null_mut(),
        );
        if device_hr != S_OK || device.is_null() {
            return Err(format!("D3D11CreateDevice failed: {device_hr:#x}"));
        }
        Ok(wio::com::ComPtr::from_raw(device))
    }
}

/// ロード済みモジュールのフルパスを返す。未ロードや取得失敗では`None`とする。
fn module_path(module: &str) -> Option<String> {
    let wide: Vec<u16> = module.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let handle = winapi::um::libloaderapi::GetModuleHandleW(wide.as_ptr());
        if handle.is_null() {
            return None;
        }
        let mut buffer = vec![0u16; 1024];
        let length = winapi::um::libloaderapi::GetModuleFileNameW(
            handle,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        );
        if length == 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buffer[..length as usize]))
    }
}
