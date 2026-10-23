/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Windows専用のGPUコピー経路（E2-blit方式）。
//!
//! ページはservo自前の通常GLテクスチャ（`CopyReady::page_texture`）へ描き、
//! end_frameでE3裏付け（XRデバイス上のMISC_SHAREDなT＋ANGLE取り込み面S）へ
//! `glBlitFramebuffer` で写し、`CopyResource(T → XR画像)` でXR画像へ渡す。
//! CPUへの読み戻し（STAGING・再アップロード）は転送経路に存在しない。
//! 同期は `glFinish` → blit → `glFinish` → `CopyResource` → `Flush` のみであり、
//! keyed mutexは使わない（E3で通ったMISC_SHARED構成）。
//! 無限待ちは使わない（`wait_image`のみ有限タイムアウトで切り上げ、そのフレームを落とす）。
//!
//! 旧E2（取り込み面のGLテクスチャへFBOで描く）は `readPixels` が `[0,0,0,0]` で
//! 描けていなかったため使わない。T1/E1-dance/T2/E3の診断経路は保持するが、
//! 既定のE2転送は本モジュールのblit経路である。上下反転はblit矩形で吸収する
//! （既定は非反転の等倍、`XR_BLIT_FLIP=1` でY反転）。
//!
//! Tは `D3D11_RESOURCE_MISC_SHARED` で作る（E3と同一）。keyed mutex付きの旧E2用Tは
//! 診断（T1/T2/E1-dance）のために残すが、既定転送には使わない。

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use glow::HasContext as _;
use log::{error, info};
use openxr::Graphics;
use surfman::{
    Context as SurfmanContext, Device as SurfmanDevice, Surface as SurfmanSurface, SurfaceTexture,
};
use webxr_api::{ContextId, Error};
use winapi::Interface as _;
use winapi::shared::dxgi::{
    IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIKeyedMutex, IDXGIResource,
};
use winapi::shared::winerror::S_OK;
use winapi::um::d3d11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_RENDER_TARGET_VIEW_DESC, D3D11_RTV_DIMENSION_TEXTURE2D,
    D3D11_RTV_DIMENSION_TEXTURE2DMS, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Device,
    ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Resource, ID3D11Texture2D,
};
use wio::com::ComPtr;

use super::graphics_d3d11::{self, Backend};
use crate::SurfmanGL;

/// `wait_image` の有限待ち時間（ナノ秒）。
///
/// acquire直後のGPUパイプライン待ちであり、通常は数ミリ秒で復帰する。
/// タイムアウト時はそのフレームを落としてセッションを継続する。
// ADR: `INFINITE`を使うとランタイム異常時にGLスレッドが永久停止するため上限を設ける。
pub(super) const WAIT_TIMEOUT_NANOS: i64 = 500_000_000;

/// keyed mutex確保の有限待ち時間（ミリ秒）。
///
/// `glFinish` 済みのため通常は即時取得できる。ANGLE側が持ち切りのときは
/// そのフレームを落として継続し、GLスレッドを止めない。
// ADR: surfman 0.14はbind/unbindで `AcquireSync(0, INFINITE)`／`ReleaseSync(0)` を使うが、
// ここではランタイム異常時の永久停止を避けるため有限値とする。キーはsurfmanに合わせた0である。
const KEYED_MUTEX_TIMEOUT_MS: u32 = 500;

/// `CopyResource` 直前にTの所有権をD3D側へ移す。
///
/// ANGLE描画は `glFinish` で確定済みであることを呼び出し元が保証する。
/// 取得失敗時は転送を行わず、そのフレームを落とす。
fn acquire_shared_texture(ready: &CopyReady) -> Result<(), i32> {
    if ready.keyed_mutex.is_null() {
        return Err(winapi::shared::winerror::E_FAIL);
    }
    // SAFETY: setupで取得した有効なkeyed mutexに対する有限待ちの確保要求である。
    let hr = unsafe { (*ready.keyed_mutex).AcquireSync(0, KEYED_MUTEX_TIMEOUT_MS) };
    if hr == S_OK { Ok(()) } else { Err(hr) }
}

/// `CopyResource`＋`Flush` 直後にTの所有権をANGLE側へ戻す。
fn release_shared_texture(ready: &CopyReady) -> Result<(), i32> {
    if ready.keyed_mutex.is_null() {
        return Err(winapi::shared::winerror::E_FAIL);
    }
    // SAFETY: acquire済みの有効なkeyed mutexに対する解放要求である。
    let hr = unsafe { (*ready.keyed_mutex).ReleaseSync(0) };
    if hr == S_OK { Ok(()) } else { Err(hr) }
}

/// 統計ログの出力間隔（end_frame回数）。
// ADR: 毎フレーム出力すると90Hzで肥大化するため、60フレームごとの集約とする。
const EMIT_EVERY_FRAMES: u64 = 60;

/// GPUコピー経路を使うかどうか。既定で使い、`XR_LAYER_MODE=import` のときだけ従来経路に戻す。
pub(super) fn copy_mode_enabled() -> bool {
    std::env::var("XR_LAYER_MODE")
        .map(|v| v != "import")
        .unwrap_or(true)
}

pub(super) fn emit_json(value: serde_json::Value) {
    let line = serde_json::to_string(&value)
        .unwrap_or_else(|_| "{\"event\":\"xr-copy-encode-error\"}".to_owned());
    info!("[xr-copy] {line}");
    // ADR: 既定の出力先はカレントディレクトリの`xr-copy.jsonl`とし、
    // `XR_COPY_LOG`で上書き・空文字で無効化する（probeの`XR_PROBE_LOG`と同一方針）。
    let path = std::env::var("XR_COPY_LOG").unwrap_or_else(|_| "xr-copy.jsonl".to_owned());
    if path.is_empty() {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write as _;
        let _ = writeln!(file, "{line}");
    }
}

/// 失敗分岐の `emit_json` を絞る。初回 3 回と、その後 600 回に 1 回だけ出す。
///
/// 90Hz で毎フレーム出すとログが暴走するため、成功経路の診断 (`DIAG_SEQ`) と同じ間隔に揃える。
fn should_emit_fail(seq: u64) -> bool {
    seq < 3 || seq % 600 == 0
}

/// デバイスのポインタ値を JSON 用の16進文字列にする。
fn ptr_str(p: *mut c_void) -> String {
    format!("{:#x}", p as usize)
}

/// D3D11 デバイスのアダプタ LUID を DXGI 経由で取得する。取得失敗時は `None` とする。
///
/// 画素には触らず、デバイスの同一性判定だけに使う。
fn device_luid(device: *mut ID3D11Device) -> Option<(u32, i32)> {
    if device.is_null() {
        return None;
    }
    // SAFETY: 呼び出し元が保証する有効なデバイスに対する問い合わせである。
    unsafe {
        let mut dxgi_device: *mut IDXGIDevice = ptr::null_mut();
        let qi_hr = (*device).QueryInterface(
            &IDXGIDevice::uuidof(),
            &mut dxgi_device as *mut *mut IDXGIDevice as *mut *mut c_void,
        );
        if qi_hr != S_OK || dxgi_device.is_null() {
            return None;
        }
        let dxgi_device = ComPtr::from_raw(dxgi_device);
        let mut adapter: *mut IDXGIAdapter = ptr::null_mut();
        if dxgi_device.GetAdapter(&mut adapter) != S_OK || adapter.is_null() {
            return None;
        }
        let adapter = ComPtr::from_raw(adapter);
        let mut adapter1: *mut IDXGIAdapter1 = ptr::null_mut();
        if adapter.QueryInterface(
            &IDXGIAdapter1::uuidof(),
            &mut adapter1 as *mut *mut IDXGIAdapter1 as *mut *mut c_void,
        ) != S_OK ||
            adapter1.is_null()
        {
            return None;
        }
        let adapter1 = ComPtr::from_raw(adapter1);
        let mut desc: winapi::shared::dxgi::DXGI_ADAPTER_DESC1 = std::mem::zeroed();
        if adapter1.GetDesc1(&mut desc) != S_OK {
            return None;
        }
        Some((desc.AdapterLuid.LowPart, desc.AdapterLuid.HighPart))
    }
}

/// XR スワップチェーン画像の所有デバイスを `GetDevice` で取得する。
///
/// `OpenSharedResource`・RTV・`CopyResource` はすべてこのデバイス上で行う必要がある。
/// 戻り値は +1 参照であり、呼び出し元が所有権を引き継ぐ。失敗時は null を返す。
fn xr_device_from_image(image: *mut ID3D11Texture2D) -> *mut ID3D11Device {
    if image.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: enumerate_images で得た有効な XR 画像に対する問い合わせである。
    unsafe {
        let mut device: *mut ID3D11Device = ptr::null_mut();
        (*image).GetDevice(&mut device);
        device
    }
}

/// 3 つのデバイスの同一性を数値で記録する。Stage A の device removed 切り分け用の一時的な観測点である。
fn emit_device_identity(
    session_device: *mut ID3D11Device,
    copy_device: *mut ID3D11Device,
    xr_image_device: *mut ID3D11Device,
) {
    let session_luid = device_luid(session_device);
    let copy_luid = device_luid(copy_device);
    let xr_luid = device_luid(xr_image_device);
    emit_json(serde_json::json!({
        "event": "device-identity",
        "session_device": ptr_str(session_device as *mut c_void),
        "copy_device": ptr_str(copy_device as *mut c_void),
        "xr_image_device": ptr_str(xr_image_device as *mut c_void),
        "same_session_copy": !session_device.is_null() && session_device == copy_device,
        "same_session_xr": !session_device.is_null() && session_device == xr_image_device,
        "same_copy_xr": !copy_device.is_null() && copy_device == xr_image_device,
        "session_luid": session_luid.map(|(low, high)| serde_json::json!({"low": low, "high": high})).unwrap_or(serde_json::Value::Null),
        "copy_luid": copy_luid.map(|(low, high)| serde_json::json!({"low": low, "high": high})).unwrap_or(serde_json::Value::Null),
        "xr_luid": xr_luid.map(|(low, high)| serde_json::json!({"low": low, "high": high})).unwrap_or(serde_json::Value::Null),
    }));
}

/// 層ごとの描画先の裏付け。GPUコピー経路を使うかどうかを型で区別する。
pub(super) enum CopyBacking {
    /// GPUコピー経路が使える層。XRデバイス上の共有テクスチャTとANGLEへ取り込んだ面Sを保持する。
    Ready(CopyReady),
    /// 初期化に失敗した層。begin_frameでエラーを返し、panicさせずにそのフレームを落とす。
    Broken,
}

pub(super) struct CopyReady {
    /// 取り込み面Sから作った描画用テクスチャ。旧E2/E1-danceの診断用に残す。
    /// 既定のE2-blit転送では使わない（FBO経由で描けないことが確定したため）。
    /// 不変条件: 通常時は `Some` であり、E1描画の途中の瞬間だけ `None` である。
    pub(super) surface_texture: Option<SurfaceTexture>,
    pub(super) gl_texture: glow::NativeTexture,
    pub(super) gl_target: u32,
    /// ページの描画先となるservo自前の通常GLテクスチャ。`SubImage.color_texture` に渡す。
    /// E2-blitの転送元であり、ページは今まで通り `XRWebGLLayer.framebuffer` に描く。
    /// 不変条件: 既定転送では `Some` であり、生成失敗時はsetup全体がBrokenになる。
    /// GLスレッドでのみ操作する。
    pub(super) page_texture: Option<glow::NativeTexture>,
    /// `page_texture` のテクスチャ標的。常に `TEXTURE_2D` である。
    pub(super) page_target: u32,
    /// 転送元の共有テクスチャ。E2ではXRデバイス上で作ったTそのものである。
    /// GLスレッドでのみ操作する。
    pub(super) shared_texture: *mut ID3D11Texture2D,
    /// Tのkeyed mutex。+1参照であり、GLスレッドでのみ操作する。
    /// `CopyResource` 前後で `AcquireSync(0)`／`ReleaseSync(0)` し、ANGLE描画との所有権を受け渡す。
    /// キーはsurfman 0.14のANGLE実装（`device.rs` のbind/unbind）に合わせた0である。
    /// 不変条件: nullまたは有効な+1参照であり、E2/T1/E1-danceの転送元がTのときだけ使う。
    pub(super) keyed_mutex: *mut IDXGIKeyedMutex,
    /// XRデバイスのimmediate context。GLスレッドでのみ操作する。
    pub(super) imm_context: *mut ID3D11DeviceContext,
    /// XRデバイス。`GetDeviceRemovedReason`での故障検出に使う。
    pub(super) d3d_device: *mut ID3D11Device,
    /// XR画像の形式。`ResolveSubresource`に渡す。
    pub(super) dst_format: u32,
    /// 診断用に保持するTの共有ハンドルと層の寸法。
    /// `GetSharedHandle` での再取得値と比較する。
    /// ADR: 診断専用の保持であり、転送経路には使わない。
    pub(super) share_handle: *mut c_void,
    pub(super) width: i32,
    pub(super) height: i32,
    /// T2の中間テクスチャ。XRデバイス上で作った87の自前資源であり、共有ハンドルの
    /// 由来物ではない。`CopyResource(T → XR画像)` の合法性だけを切り分ける。
    /// 不変条件: nullまたは有効な+1参照であり、GLスレッドでのみ操作する。
    pub(super) t2_texture: *mut ID3D11Texture2D,
    /// E3の裏付け（XRデバイス作成T＋ANGLE取り込み面S）。遅延生成する。
    pub(super) e3: Option<E3State>,
    /// XR画像がMSAAのときに`CopyResource`ではなく`ResolveSubresource`を使う。
    pub(super) resolve: bool,
    /// acquire済みで未releaseのXR画像index。GLスレッドでのみ操作する。
    pub(super) acquired_image: Option<u32>,
}

// 生ポインタ群は自動的にSendである。OpenXrLayerManagerがGLスレッドに閉じるため追加の同期は行わない。
// ADR: SurfaceTextureの破棄にはdevice/contextが必要なためDropではなくdestroy_backingで明示解放する.

/// GLエラーキューを上限付きで排出する。
///
/// three.js等がセッション開始前に残した既存エラーを、直後の自前の呼び出しの
/// 失敗と誤認しないための一段である。GLエラーはキューに溜まるため、事前に
/// 空にしないと `tex_image_2d` 直後の `get_error` が他者の残留を拾う。
/// 戻り値は排出したエラー値の一覧であり、空なら残留が無い。
// ADR: 無限ループ防止の上限16は、仕様上のキュー深さに対して十分な余裕である。
fn drain_gl_errors(gl: &glow::Context) -> Vec<u32> {
    let mut drained = Vec::new();
    // SAFETY: GLスレッド上の有効なコンテキストに対する状態取得であり、描画内容は変えない。
    unsafe {
        for _ in 0..16 {
            let err = gl.get_error();
            if err == glow::NO_ERROR {
                break;
            }
            drained.push(err);
        }
    }
    drained
}

/// ページ描画先テクスチャを作る。残留GLエラーを先に捨て、失敗時は代替方式へ切り替える。
///
/// 戻り値は (テクスチャ, 通った方式名) である。方式名は `xr-copy-setup-page` の
/// `method` と `xr-copy-setup` (e2-blit) の `page_method` に記録する。
/// 失敗時は全試行のエラー・渡した引数・上限値・束縛状態を含む理由文を返す。
/// CPU読み戻しは行わない。
// ADR: XRift実測で `tex_image_2d` が 0x501 (GL_INVALID_VALUE) になるが、同一寸法が
// `xr-min.html` では通る。three.jsが事前に残したエラーの誤認か、コンテキスト状態の
// 差異かを切り分けるため、事前ドレイン・上限値・代替保管方式の記録を一段で行う。
// 代替は可変保管 (`tex_image_2d`) から不変保管 (`tex_storage_2d`) へ切り替え、
// 内部形式も RGBA8 から SRGB8_ALPHA8・無サイズRGBAへ順に試す。
fn create_page_texture(
    gl: &glow::Context,
    width: i32,
    height: i32,
    context_id: ContextId,
) -> Result<(glow::NativeTexture, &'static str), String> {
    // SAFETY: 以下のGL呼び出し群はGLスレッド上の有効なコンテキストに対するものであり、
    // 既存の描画内容は変えず、FBOの結び替えは元に戻す。診断用の一時テクスチャは失敗時に都度捨てる。
    unsafe {
        let gl_ptr = gl as *const glow::Context as usize;
        // 事前ドレイン。three.js側の残留をここで数える。
        let drained_pre = drain_gl_errors(gl);
        // 上限値と呼び出し時の束縛状態を記録する。取得自体がエラーを積む場合に備え、
        // 取得後にキューをもう一度捨て、転送用の `tex_image_2d`/`tex_storage_2d` の判定を汚さない。
        // 取得由来の排出は `drained_query` として区別する。
        let max_texture = gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE);
        let max_renderbuffer = gl.get_parameter_i32(glow::MAX_RENDERBUFFER_SIZE);
        let mut max_viewport = [0; 2];
        gl.get_parameter_i32_slice(glow::MAX_VIEWPORT_DIMS, &mut max_viewport);
        let active_unit = gl.get_parameter_i32(glow::ACTIVE_TEXTURE);
        let bound_2d = gl.get_parameter_i32(glow::TEXTURE_BINDING_2D);
        let draw_fbo = gl.get_parameter_framebuffer(glow::DRAW_FRAMEBUFFER_BINDING);
        let read_fbo = gl.get_parameter_framebuffer(glow::READ_FRAMEBUFFER_BINDING);
        let drained_query = drain_gl_errors(gl);
        let caps = serde_json::json!({
            "max_texture_size": max_texture,
            "max_renderbuffer_size": max_renderbuffer,
            "max_viewport_dims": max_viewport,
            "active_texture": format!("{active_unit:#x}"),
            "bound_texture_2d": bound_2d,
            "draw_fbo": format!("{draw_fbo:?}"),
            "read_fbo": format!("{read_fbo:?}"),
            "context_id": context_id.0,
            "gl_ptr": format!("{gl_ptr:#x}"),
        });
        let mut attempts: Vec<serde_json::Value> = Vec::new();
        // 試行の定義は (方式名, 内部形式, 不変保管か) である。引数の形式・型・境界は全試行で共通とする。
        // 内部形式の数値はログ上で10進と16進の両方を残す。
        let formats: [(&'static str, u32, bool); 4] = [
            ("tex_image_rgba8", glow::RGBA8, false),
            ("tex_storage_rgba8", glow::RGBA8, true),
            ("tex_storage_srgb8_alpha8", glow::SRGB8_ALPHA8, true),
            ("tex_image_rgba_unsized", glow::RGBA, false),
        ];
        for (method, internal, immutable) in formats {
            let tex = match gl.create_texture() {
                Ok(tex) => tex,
                Err(e) => {
                    attempts.push(serde_json::json!({
                        "method": method,
                        "internal_format": internal,
                        "internal_format_hex": format!("{internal:#x}"),
                        "error": format!("create_texture failed: {e:?}"),
                    }));
                    continue;
                },
            };
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::NEAREST as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::NEAREST as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            if immutable {
                gl.tex_storage_2d(glow::TEXTURE_2D, 1, internal, width, height);
            } else {
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    internal as i32,
                    width,
                    height,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(None),
                );
            }
            gl.bind_texture(glow::TEXTURE_2D, None);
            let err = gl.get_error();
            if err == glow::NO_ERROR {
                emit_json(serde_json::json!({
                    "event": "xr-copy-setup-page",
                    "ok": true,
                    "method": method,
                    "width": width,
                    "height": height,
                    "internal_format": internal,
                    "internal_format_hex": format!("{internal:#x}"),
                    "format": glow::RGBA,
                    "format_hex": format!("{:#x}", glow::RGBA),
                    "type": glow::UNSIGNED_BYTE,
                    "type_hex": format!("{:#x}", glow::UNSIGNED_BYTE),
                    "border": 0,
                    "caps": caps,
                    "drained_pre": drained_pre.iter().map(|e| format!("{e:#x}")).collect::<Vec<_>>(),
                    "drained_pre_count": drained_pre.len(),
                    "drained_query": drained_query.iter().map(|e| format!("{e:#x}")).collect::<Vec<_>>(),
                    "attempts": attempts,
                }));
                return Ok((tex, method));
            }
            gl.delete_texture(tex);
            attempts.push(serde_json::json!({
                "method": method,
                "internal_format": internal,
                "internal_format_hex": format!("{internal:#x}"),
                "error": format!("{err:#x}"),
            }));
        }
        emit_json(serde_json::json!({
            "event": "xr-copy-setup-page",
            "ok": false,
            "width": width,
            "height": height,
            "format": glow::RGBA,
            "format_hex": format!("{:#x}", glow::RGBA),
            "type": glow::UNSIGNED_BYTE,
            "type_hex": format!("{:#x}", glow::UNSIGNED_BYTE),
            "border": 0,
            "caps": caps,
            "drained_pre": drained_pre.iter().map(|e| format!("{e:#x}")).collect::<Vec<_>>(),
            "drained_pre_count": drained_pre.len(),
            "drained_query": drained_query.iter().map(|e| format!("{e:#x}")).collect::<Vec<_>>(),
            "attempts": attempts,
        }));
        let drained_hex: Vec<String> =
            drained_pre.iter().map(|e| format!("{e:#x}")).collect();
        Err(format!(
            "page texture failed: all methods failed width={width} height={height} \
            drained_pre={drained_hex:?}({}) caps={caps} attempts={attempts:?}",
            drained_pre.len(),
        ))
    }
}

/// 層のGPUコピー用裏付けを作る。失敗時は [`CopyBacking::Broken`] を返す。
///
/// E2（既定動作）: XRデバイス上に共有テクスチャTを作り、その共有ハンドルを
    /// ANGLEデバイス上で開き直して `create_surface_texture_from_texture` で
    /// 取り込んだ面Sへ直接結び付けた描画用テクスチャを得る。ページはこのGLテクスチャへ
/// 直接描き、end_frameで `CopyResource(T → XR画像)` する。TとXR画像は同一デバイス上の
/// 資源であるためデバイス間転送は発生しない。
/// ADR: ANGLEが作ったpbufferをXR側で開く旧経路は黒になるため使わない（T1/E1'で確定）。
pub(super) fn setup_backing(
    contexts: &mut dyn webxr_api::GLContexts<SurfmanGL>,
    context_id: ContextId,
    images: &[<Backend as Graphics>::SwapchainImage],
    width: i32,
    height: i32,
    format: u32,
    sample_count: u32,
) -> CopyBacking {
    let fail = |reason: String| {
        error!("[xr-copy] setup failed: {reason}");
        emit_json(serde_json::json!({"event": "xr-copy-setup-failed", "reason": reason}));
        CopyBacking::Broken
    };
    let Some(device) = contexts.device(context_id) else {
        return fail("no device for context".into());
    };
    let size = euclid::Size2D::new(width, height);
    // ANGLE 側デバイス（取り込み用）と XR 画像の所有デバイスを区別する。
    // Tの生成・`CopyResource` はすべて XR 画像と同一デバイス上で行う。
    // そうでないと転送が無効化（黒のまま）し、XR 画像への RTV/Clear が device removed を誘発する。
    // ADR: XR 画像の `GetDevice` を正とし、セッション生成時の保持ポインタと突き合わせて数値で確定する。
    let native = device.native_device();
    let angle_device = native.d3d11_device;
    if angle_device.is_null() {
        return fail("ANGLE d3d11_device is null".into());
    }
    let first_image = images.first().copied().unwrap_or(ptr::null_mut());
    if first_image.is_null() {
        // SAFETY: setup内で取得した+1参照をここで放す。
        unsafe { drop(ComPtr::from_raw(angle_device)) };
        return fail("swapchain has no images".into());
    }
    // SAFETY: enumerate_imagesで得た有効なXR画像であり、GetDeviceの出力先は有効である。
    // openxr-sysのD3D型は不透明なc_voidのためwinapi型へcastする（既存経路と同一）。
    let first_texture = first_image as *mut ID3D11Texture2D;
    let xr_device = xr_device_from_image(first_texture);
    if xr_device.is_null() {
        // SAFETY: setup内で取得した+1参照をここで放す。
        unsafe { drop(ComPtr::from_raw(angle_device)) };
        return fail("GetDevice on XR image returned null".into());
    }
    let session_device = graphics_d3d11::session_device_ptr();
    emit_device_identity(
        session_device,
        angle_device,
        xr_device,
    );
    // TをXRデバイス上に作る（E3と同一）。層寸法とXR画像寸法の不一致は転送不能のためBrokenとする。
    // keyed mutex付きで作り、転送前後のAcquire/Release(0)に使うmutexも返す。
    let (t, keyed_mutex, t_handle) = unsafe {
        let mut image_desc: D3D11_TEXTURE2D_DESC = std::mem::zeroed();
        (*first_texture).GetDesc(&mut image_desc);
        if image_desc.Width != width as u32 || image_desc.Height != height as u32 {
            drop(ComPtr::from_raw(angle_device));
            drop(ComPtr::from_raw(xr_device));
            return fail(format!(
                "layer size {width}x{height} != swapchain image {}x{}",
                image_desc.Width, image_desc.Height,
            ));
        }
        let t_desc = D3D11_TEXTURE2D_DESC {
            Width: image_desc.Width,
            Height: image_desc.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: winapi::shared::dxgiformat::DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: winapi::shared::dxgitype::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: winapi::um::d3d11::D3D11_USAGE_DEFAULT,
            // SHADER_RESOURCEはANGLEが取り込み面へBindTexImageする際のSRV生成に要る。
            // 無いとSRV生成がE_INVALIDARGで失敗し、描画用テクスチャが作れない。
            BindFlags: winapi::um::d3d11::D3D11_BIND_RENDER_TARGET |
                winapi::um::d3d11::D3D11_BIND_SHADER_RESOURCE,
            CPUAccessFlags: 0,
            // KEYEDMUTEXはANGLE取り込みの前提である。surfman 0.14のANGLE実装は
            // `EGL_DXGI_KEYED_MUTEX_ANGLE` を問い合わせて `Synchronization::KeyedMutex` とし、
            // bind/unbindでAcquire/Release(0)する。MISC_SHAREDだけではSRV生成が
            // `Error allocating ShaderResourceView` (E_INVALIDARG) で失敗する。
            MiscFlags: winapi::um::d3d11::D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
        };
        let mut t: *mut ID3D11Texture2D = ptr::null_mut();
        // SAFETY: XRデバイス上の有効な生成要求であり、出力先は有効である。
        let create_hr = (*xr_device).CreateTexture2D(&t_desc, ptr::null(), &mut t);
        if create_hr != S_OK || t.is_null() {
            drop(ComPtr::from_raw(angle_device));
            drop(ComPtr::from_raw(xr_device));
            return fail(format!("E2 CreateTexture2D for T failed: {create_hr:#x}"));
        }
        // Tのkeyed mutexを取得する。転送前後のAcquire/Release(0)に使う。
        // 取得できないTではANGLEのSRV生成が通らないため、ここでBrokenとする。
        // SAFETY: 生成したばかりの有効なテクスチャに対する問い合わせであり、出力先は有効である。
        let keyed_mutex = unsafe {
            let mut mutex_: *mut IDXGIKeyedMutex = ptr::null_mut();
            let qi_hr = (*t).QueryInterface(
                &IDXGIKeyedMutex::uuidof(),
                &mut mutex_ as *mut *mut IDXGIKeyedMutex as *mut *mut c_void,
            );
            if qi_hr != S_OK || mutex_.is_null() {
                drop(ComPtr::from_raw(t));
                drop(ComPtr::from_raw(angle_device));
                drop(ComPtr::from_raw(xr_device));
                return fail(format!("E2 QueryInterface IDXGIKeyedMutex failed: {qi_hr:#x}"));
            }
            mutex_
        };
        // 共有ハンドルを取得する。
        // SAFETY: 生成したばかりの有効なテクスチャに対する問い合わせである。
        let mut res: *mut IDXGIResource = ptr::null_mut();
        let qi_hr = (*t).QueryInterface(
            &IDXGIResource::uuidof(),
            &mut res as *mut *mut IDXGIResource as *mut *mut c_void,
        );
        if qi_hr != S_OK || res.is_null() {
            drop(ComPtr::from_raw(t));
            drop(ComPtr::from_raw(keyed_mutex));
            drop(ComPtr::from_raw(angle_device));
            drop(ComPtr::from_raw(xr_device));
            return fail(format!("E2 QueryInterface IDXGIResource failed: {qi_hr:#x}"));
        }
        let res = ComPtr::from_raw(res);
        let mut handle: *mut c_void = ptr::null_mut();
        let handle_hr = res.GetSharedHandle(&mut handle);
        if handle_hr != S_OK || handle.is_null() {
            drop(ComPtr::from_raw(t));
            drop(ComPtr::from_raw(keyed_mutex));
            drop(ComPtr::from_raw(angle_device));
            drop(ComPtr::from_raw(xr_device));
            return fail(format!("E2 GetSharedHandle failed: {handle_hr:#x}"));
        }
        (t, keyed_mutex, handle)
    };
    // Tの共有ハンドルをANGLEデバイス上で開き、面Sとして取り込む（E3と同一手順）。
    // 以降の失敗では T(+1)・mutex(+1)・angle(+1)・xr(+1) を残さない。
    // SAFETY: native_device()は+1参照で返すためComPtrで引き継ぎ、開いたテクスチャ(+1)ごと
    // create_surface_from_textureへ所有権を渡す。Err時は被呼出側で破棄される。
    // surfmanのcontext借用はこの区間に閉じる。後のbindings借用と重ねない。
    let surface_texture = {
        let Some(context) = contexts.context(context_id) else {
            unsafe {
                drop(ComPtr::from_raw(angle_device));
                drop(ComPtr::from_raw(t));
                drop(ComPtr::from_raw(keyed_mutex));
                drop(ComPtr::from_raw(xr_device));
            }
            return fail("no surfman context".into());
        };
        unsafe {
            let _angle_guard = ComPtr::from_raw(angle_device);
            let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
            let open_hr = (*angle_device).OpenSharedResource(
                t_handle,
                &ID3D11Texture2D::uuidof(),
                &mut tex as *mut *mut ID3D11Texture2D as *mut *mut std::ffi::c_void,
            );
            if open_hr != S_OK || tex.is_null() {
                drop(ComPtr::from_raw(t));
                drop(ComPtr::from_raw(keyed_mutex));
                drop(ComPtr::from_raw(xr_device));
                return fail(format!("E2 OpenSharedResource on ANGLE failed: {open_hr:#x}"));
            }
            // EGL面を共有ハンドル経由で作り直さず、取り込み面へ直接BindTexImageする。
            // create_surface_textureは共有ハンドルの再importを伴い、
            // XRデバイス製TではBadParameterになるため使わない。
            // ADR: 面Sの再importではなく、取り込み面そのものへ結び付ける一段である。
            match device.create_surface_texture_from_texture(context, &size, ComPtr::from_raw(tex)) {
                Ok(surface_texture) => surface_texture,
                Err(e) => {
                    drop(ComPtr::from_raw(t));
                    drop(ComPtr::from_raw(keyed_mutex));
                    drop(ComPtr::from_raw(xr_device));
                    return fail(format!("E2 create_surface_texture_from_texture failed: {e:?}"));
                },
            }
        }
    };
    let Some(gl_texture) = device.surface_texture_object(&surface_texture) else {
        if let Some(context) = contexts.context(context_id) {
            destroy_surface_texture_quiet(&device, context, surface_texture);
        } else {
            std::mem::forget(surface_texture);
        }
        // SAFETY: setup内で取得した+1参照をここで放す。
        unsafe {
            drop(ComPtr::from_raw(t));
            drop(ComPtr::from_raw(keyed_mutex));
            drop(ComPtr::from_raw(xr_device));
        }
        return fail("imported surface texture has no GL object".into());
    };
    let gl_target = device.surface_gl_texture_target();
    // 以降は XR デバイスに統一する。ANGLE 側の +1 参照は取り込み時に放済みである。
    let d3d_device = xr_device;
    let mut imm: *mut ID3D11DeviceContext = ptr::null_mut();
    // SAFETY: 出力先ポインタは有効であり、取得したimmediate contextは+1参照として保持する。
    unsafe { (*d3d_device).GetImmediateContext(&mut imm) };
    if imm.is_null() {
        // SAFETY: setup内で取得した+1参照をここで放す。
        unsafe {
            drop(ComPtr::from_raw(t));
            drop(ComPtr::from_raw(keyed_mutex));
            drop(ComPtr::from_raw(d3d_device));
        }
        if let Some(context) = contexts.context(context_id) {
            destroy_surface_texture_quiet(&device, context, surface_texture);
        } else {
            std::mem::forget(surface_texture);
        }
        return fail("GetImmediateContext on XR device returned null".into());
    }
    // 転送元と転送先の記述子を取得する。画素は読まない。
    let mut src_desc: D3D11_TEXTURE2D_DESC = unsafe { std::mem::zeroed() };
    // SAFETY: tはXRデバイス上で作った有効なテクスチャである。
    unsafe { (*t).GetDesc(&mut src_desc) };
    let mut dst_desc: D3D11_TEXTURE2D_DESC = unsafe { std::mem::zeroed() };
    // SAFETY: setup先頭で有効性を確認したXR画像の記述子取得である。
    unsafe { (*first_texture).GetDesc(&mut dst_desc) };
    // 転送可否を記述子で事前判定する。寸法・形式の不一致やMSAA構成の想定外はBrokenとする。
    let dims_match =
        dst_desc.Width == src_desc.Width && dst_desc.Height == src_desc.Height;
    // ADR: XRランタイムはSRGBビュー用にTYPELESS(90)で画像を作ることがある。
    // 転送元87(UNORM)→転送先90(TYPELESS)は同一32bitレイアウトのためCopyResourceで渡せる。
    // 形式変換は行わず、ビット列をそのまま写す（sRGB解釈の差異はログに残すのみ）。
    let format_match = dst_desc.Format == src_desc.Format ||
        (src_desc.Format ==
            winapi::shared::dxgiformat::DXGI_FORMAT_B8G8R8A8_UNORM &&
            dst_desc.Format == winapi::shared::dxgiformat::DXGI_FORMAT_B8G8R8A8_TYPELESS);
    let format_exact = dst_desc.Format == src_desc.Format;
    let resolve = dims_match &&
        format_match &&
        dst_desc.SampleDesc.Count > 1 &&
        src_desc.SampleDesc.Count == 1;
    let copy = dims_match && format_match && dst_desc.SampleDesc.Count == 1;
    emit_json(serde_json::json!({
        "event": "xr-copy-setup",
        "mode": "gpu-copy-e2",
        "width": width,
        "height": height,
        "format": format,
        "is_srgb": format == winapi::shared::dxgiformat::DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
        "recommended_sample_count": sample_count,
        "t_handle": format!("{:#x}", t_handle as usize),
        "src": {"width": src_desc.Width, "height": src_desc.Height,
                "format": src_desc.Format, "samples": src_desc.SampleDesc.Count,
                "bind_flags": src_desc.BindFlags, "misc_flags": src_desc.MiscFlags,
                "usage": src_desc.Usage, "cpu_access": src_desc.CPUAccessFlags,
                "mips": src_desc.MipLevels, "array": src_desc.ArraySize,
                "quality": src_desc.SampleDesc.Quality},
        "dst": {"width": dst_desc.Width, "height": dst_desc.Height,
                "format": dst_desc.Format, "samples": dst_desc.SampleDesc.Count,
                "bind_flags": dst_desc.BindFlags, "misc_flags": dst_desc.MiscFlags,
                "usage": dst_desc.Usage, "cpu_access": dst_desc.CPUAccessFlags,
                "mips": dst_desc.MipLevels, "array": dst_desc.ArraySize,
                "quality": dst_desc.SampleDesc.Quality},
        "transfer": if copy {"copy"} else if resolve {"resolve"} else {"incompatible"},
        "format_exact": format_exact,
    }));
    if !copy && !resolve {
        // SAFETY: setup内で取得した+1参照をここで放す。
        unsafe {
            drop(ComPtr::from_raw(t));
            drop(ComPtr::from_raw(keyed_mutex));
            drop(ComPtr::from_raw(imm));
            drop(ComPtr::from_raw(d3d_device));
        }
        if let Some(context) = contexts.context(context_id) {
            destroy_surface_texture_quiet(&device, context, surface_texture);
        } else {
            std::mem::forget(surface_texture);
        }
        return fail("src/dst descriptors are incompatible for GPU copy".into());
    }
    // E2-blit用のページ描画先（servo自前の通常GLテクスチャ）を作る。
    // ページはこのテクスチャへ今まで通り `XRWebGLLayer.framebuffer` に描く。
    // E2以前に色が出ていた健全な経路であり、FBO経由で描けない取り込み面は使わない。
    // ADR: 取り込み面のGLテクスチャへFBOで描く方式は `readPixels [0,0,0,0]` で死角のため、
    // ページ描画と転送元（E3のT）を分離し、間を `glBlitFramebuffer` で繋ぐ。
    // surfmanのcontext借用とbindings借用を重ねないため、生成だけを区間に閉じる。
    // 失敗時の後片付けは借用終了後に行う。
    let page_tex_result: Result<(glow::NativeTexture, &'static str), String> = {
        let Some(gl) = contexts.bindings(context_id) else {
            // bindings借用は成立していないため、後片付けでcontextを借りてよい。
            unsafe {
                drop(ComPtr::from_raw(t));
                drop(ComPtr::from_raw(keyed_mutex));
                drop(ComPtr::from_raw(imm));
                drop(ComPtr::from_raw(d3d_device));
            }
            if let Some(context) = contexts.context(context_id) {
                destroy_surface_texture_quiet(&device, context, surface_texture);
            } else {
                std::mem::forget(surface_texture);
            }
            return fail("no GL bindings for page texture".into());
        };
        // ページ描画先の実体であり画素の読み戻しはしない。残留エラーの排出・
        // 代替保管方式の切り替え・上限値等の記録は `create_page_texture` が担う。
        // surfmanのcontext借用とbindings借用を重ねないため、生成だけを区間に閉じる。
        // 失敗時の後片付けは借用終了後に行う。
        create_page_texture(gl, width, height, context_id)
    };
    let (page_texture, page_method) = match page_tex_result {
        Ok((tex, method)) => (Some(tex), method),
        Err(reason) => {
            unsafe {
                drop(ComPtr::from_raw(t));
                drop(ComPtr::from_raw(keyed_mutex));
                drop(ComPtr::from_raw(imm));
                drop(ComPtr::from_raw(d3d_device));
            }
            if let Some(context) = contexts.context(context_id) {
                destroy_surface_texture_quiet(&device, context, surface_texture);
            } else {
                std::mem::forget(surface_texture);
            }
            return fail(reason);
        },
    };
    let page_target = glow::TEXTURE_2D;
    emit_json(serde_json::json!({
        "event": "xr-copy-setup",
        "mode": "gpu-copy-e2-blit",
        "width": width,
        "height": height,
        "page_target": page_target,
        "page_method": page_method,
    }));
    CopyBacking::Ready(CopyReady {
        surface_texture: Some(surface_texture),
        gl_texture,
        gl_target,
        page_texture,
        page_target,
        shared_texture: t,
        keyed_mutex,
        imm_context: imm,
        d3d_device,
        dst_format: dst_desc.Format,
        share_handle: t_handle,
        width,
        height,
        resolve,
        t2_texture: ptr::null_mut(),
        e3: None,
        acquired_image: None,
    })
}

/// acquire済み画像のGPUパイプライン待ち（有限）を行い、転送対象として記録する。
///
/// 待ちに失敗した画像はここでreleaseするため、呼び出し後にrelease漏れは残らない。
pub(super) fn wait_for_copy(
    swapchain: &mut openxr::Swapchain<Backend>,
    ready: &mut CopyReady,
    image: u32,
) -> Result<(), Error> {
    // 前フレームのacquireが残っている場合は再利用し、二重acquireを避ける。
    if ready.acquired_image.is_some() {
        return Ok(());
    }
    if let Err(e) = swapchain.wait_image(openxr::Duration::from_nanos(WAIT_TIMEOUT_NANOS)) {
        let _ = swapchain.release_image();
        return Err(Error::BackendSpecific(format!(
            "Swapchain::wait_image {:?}",
            e
        )));
    }
    ready.acquired_image = Some(image);
    Ok(())
}

/// 診断読み戻しの実施通番。`copy_to_swapchain` の呼び出しごとに進む。
///
/// 初回3回と、その後600回に1回だけ診断する。STAGING確保と `Map` 待ちを伴うため
/// 毎フレームは行わない。
/// ADR: 診断専用の観測点であり、転送経路には入れない。
static DIAG_SEQ: AtomicU64 = AtomicU64::new(0);

/// D3D側診断読み戻しの範囲を環境変数で切り替える。
/// `XR_COPY_DIAG_D3D` が `nodst` ならXR画像側、`nosrc` なら共有側、`nod3d` なら両方を
/// 省略する（GL側診断と転送自体は行う）。デバイス除去の切り分け用の一時的な綱である。
/// ADR: 原因特定後に削除する。
static DIAG_D3D_MODE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn diag_d3d_mode() -> &'static str {
    DIAG_D3D_MODE
        .get_or_init(|| std::env::var("XR_COPY_DIAG_D3D").unwrap_or_default())
        .as_str()
}

/// 診断で読む画素位置。左端・中央・右端の3点をGL座標（原点は左下）で返す。
/// 左端は赤帯、右端は緑帯、中央はカラーバーの想定である。
fn diag_points(width: i32, height: i32) -> [(i32, i32); 3] {
    let y = height / 2;
    [(2, y), (width / 2, y), (width - 3, y)]
}

/// 1段分の診断読み戻し結果。`pixels` は左端・中央・右端の順で、
/// GL側はRGBA、D3D側はBGRAのまま格納する。
struct DiagRead {
    value: serde_json::Value,
    pixels: Option<[[u8; 4]; 3]>,
}

/// GLテクスチャ側の診断読み戻し。FBOにアタッチして数点を読む。
/// ページの描画が `SubImage.color_texture` に届いているかを見る。
/// E2-blitでは `page_texture`（servo自前の通常テクスチャ）を読み、
/// 旧経路の診断時のみ取り込み面の `gl_texture` を読む。
/// ADR: 診断専用の読み戻しであり、転送経路には入れない。
fn diag_read_gl(gl: &glow::Context, ready: &CopyReady) -> DiagRead {
    let points = diag_points(ready.width, ready.height);
    let fail = |reason: String| DiagRead {
        value: serde_json::json!({"ok": false, "error": reason}),
        pixels: None,
    };
    // E2-blit既定ではページ描画先を読む。旧診断（E1-dance等）では取り込み面を読む。
    let (tex, target, source) = if let Some(page) = ready.page_texture {
        (page, ready.page_target, "page")
    } else {
        let Some(_surface_texture) = ready.surface_texture.as_ref() else {
            return fail("no surface_texture for GL diag".into());
        };
        (ready.gl_texture, ready.gl_target, "imported")
    };
    // SAFETY: GLスレッド上の有効なコンテキストに対する診断専用の読み戻しである。
    // 既存の描画内容は変えず、FBOの結び替えは元に戻し、診断用FBOは都度捨てる。
    unsafe {
        let draw_fbo = gl.get_parameter_framebuffer(glow::DRAW_FRAMEBUFFER_BINDING);
        let read_fbo = gl.get_parameter_framebuffer(glow::READ_FRAMEBUFFER_BINDING);
        let fbo = match gl.create_framebuffer() {
            Ok(fbo) => fbo,
            Err(e) => return fail(format!("create_framebuffer: {e:?}")),
        };
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            target,
            Some(tex),
            0,
        );
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        let mut sampled = [[0u8; 4]; 3];
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
        for (i, (x, y)) in points.iter().enumerate() {
            let mut buf = [0u8; 4];
            gl.read_pixels(
                *x,
                *y,
                1,
                1,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut buf[..])),
            );
            sampled[i] = buf;
        }
        let error = gl.get_error();
        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, draw_fbo);
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, read_fbo);
        gl.delete_framebuffer(fbo);
        DiagRead {
            value: serde_json::json!({
                "ok": true,
                "source": source,
                "left_rgba": sampled[0],
                "center_rgba": sampled[1],
                "right_rgba": sampled[2],
                "fbo_status": format!("{status:#x}"),
                "gl_error": format!("{error:#x}"),
            }),
            pixels: Some(sampled),
        }
    }
}

/// D3Dテクスチャ側の診断読み戻し。STAGINGへ転送して `Map` し、数点を読む。
/// `via_resolve` が真のときはMSAA解決で読む（XR画像側がMSAAの場合）。
/// ADR: 診断専用の読み戻しであり、転送経路には入れない。STAGINGは都度作って捨てる。
unsafe fn diag_read_d3d(
    device: *mut ID3D11Device,
    imm: *mut ID3D11DeviceContext,
    src: *mut ID3D11Texture2D,
    via_resolve: bool,
    resolve_format: u32,
) -> DiagRead {
    let fail = |reason: String| DiagRead {
        value: serde_json::json!({"ok": false, "error": reason}),
        pixels: None,
    };
    if device.is_null() || imm.is_null() || src.is_null() {
        return fail("null device/context/texture".into());
    }
    // SAFETY: 呼び出し元が保証する有効なテクスチャの記述子取得である。
    let mut desc: D3D11_TEXTURE2D_DESC = std::mem::zeroed();
    (*src).GetDesc(&mut desc);
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: desc.Width,
        Height: desc.Height,
        MipLevels: 1,
        ArraySize: 1,
        Format: desc.Format,
        SampleDesc: winapi::shared::dxgitype::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ,
        MiscFlags: 0,
    };
    let mut staging: *mut ID3D11Texture2D = ptr::null_mut();
    // SAFETY: 出力先ポインタは有効であり、STAGING記述子は読み戻し用途の定型である。
    let create_hr = (*device).CreateTexture2D(&staging_desc, ptr::null(), &mut staging);
    if create_hr != S_OK || staging.is_null() {
        return fail(format!("CreateTexture2D staging failed: {create_hr:#x}"));
    }
    // SAFETY: ここで確保した+1参照はガードが放す。GLスレッドでのみ実行される。
    let _staging_guard = ComPtr::from_raw(staging);
    // SAFETY: 転送元・転送先はいずれも有効であり、immediate context上で順序実行される。
    if via_resolve {
        (*imm).ResolveSubresource(
            staging as *mut ID3D11Resource,
            0,
            src as *mut ID3D11Resource,
            0,
            resolve_format,
        );
    } else {
        (*imm).CopyResource(
            staging as *mut ID3D11Resource,
            src as *mut ID3D11Resource,
        );
    }
    let mut mapped: D3D11_MAPPED_SUBRESOURCE = std::mem::zeroed();
    // SAFETY: STAGINGへの有効なMap要求であり、読み範囲は記述子内に収める。
    let map_hr = (*imm).Map(
        staging as *mut ID3D11Resource,
        0,
        D3D11_MAP_READ,
        0,
        &mut mapped,
    );
    if map_hr != S_OK || mapped.pData.is_null() {
        return fail(format!("Map failed: {map_hr:#x}"));
    }
    // D3Dの行0は上端のため、GL座標（原点は左下）とは上下が反転する。
    // 同一論理画素の比較になるよう行を反転して読む。
    let w = desc.Width as i32;
    let h = desc.Height as i32;
    let points = diag_points(w, h);
    let mut sampled = [[0u8; 4]; 3];
    for (i, (x, y_gl)) in points.iter().enumerate() {
        let row = (h - 1 - y_gl) as isize;
        // SAFETY: Map済み範囲内の4バイト読みであり、行列とも記述子内に収まる。
        let base =
            (mapped.pData as *const u8).offset(row * mapped.RowPitch as isize + *x as isize * 4);
        let px = std::slice::from_raw_parts(base, 4);
        sampled[i] = [px[0], px[1], px[2], px[3]];
    }
    (*imm).Unmap(staging as *mut ID3D11Resource, 0);
    DiagRead {
        value: serde_json::json!({
            "ok": true,
            "left_bgra": sampled[0],
            "center_bgra": sampled[1],
            "right_bgra": sampled[2],
            "width": desc.Width,
            "height": desc.Height,
            "format": desc.Format,
            "samples": desc.SampleDesc.Count,
            "map_hr": format!("{map_hr:#x}"),
        }),
        pixels: Some(sampled),
    }
}

/// 段ごとの診断結果を1行JSONで記録する。黒画面の切断点を数値で特定するための一時的な観測点である。
fn emit_diag(seq: u64, ready: &CopyReady, gl: DiagRead, src: DiagRead, dst: DiagRead) {
    // GL側はRGBA、D3D側はBGRAのため、比較時にチャンネルを揃える。
    let swizzle = |bgra: [u8; 4]| [bgra[2], bgra[1], bgra[0], bgra[3]];
    let match_gl_src = match (&gl.pixels, &src.pixels) {
        (Some(g), Some(s)) => g.iter().zip(s.iter()).all(|(g, s)| *g == swizzle(*s)),
        _ => false,
    };
    let match_src_dst = match (&src.pixels, &dst.pixels) {
        (Some(s), Some(d)) => s == d,
        _ => false,
    };
    // 共有ハンドルの再取得。GLテクスチャ側と共有元が同一物かを見る。
    // SAFETY: setupで開いた有効な共有テクスチャに対する問い合わせである。
    let (handle_now, handle_hr) = unsafe {
        let mut res: *mut IDXGIResource = ptr::null_mut();
        let hr = (*ready.shared_texture).QueryInterface(
            &IDXGIResource::uuidof(),
            &mut res as *mut *mut IDXGIResource as *mut *mut c_void,
        );
        if hr != S_OK || res.is_null() {
            (None, hr)
        } else {
            let res = ComPtr::from_raw(res);
            // winapi 0.3のDXGI 1.0式シグネチャではハンドルをout引数で受ける。
            let mut handle: *mut c_void = ptr::null_mut();
            let handle_hr = res.GetSharedHandle(&mut handle);
            (Some(handle as usize), handle_hr)
        }
    };
    emit_json(serde_json::json!({
        "event": "xr-copy-diag",
        "seq": seq,
        "size": {"width": ready.width, "height": ready.height},
        "gl_texture": gl.value,
        "shared_texture": src.value,
        "xr_image": dst.value,
        "match_gl_shared": match_gl_src,
        "match_shared_xr": match_src_dst,
        "share_handle": format!("{:#x}", ready.share_handle as usize),
        "share_handle_requery": handle_now.map(|h| format!("{h:#x}")),
        "share_handle_hr": format!("{handle_hr:#x}"),
        "share_handle_equal": handle_now == Some(ready.share_handle as usize),
        "resolve": ready.resolve,
    }));
}

/// XRスワップチェーン画像の記述子を診断用に返す (width, height, format, samples)。
pub(super) fn describe_image(
    image: <Backend as Graphics>::SwapchainImage,
) -> Option<(u32, u32, u32, u32)> {
    if image.is_null() {
        return None;
    }
    // SAFETY: enumerate_imagesで得た有効なXR画像の記述子取得である。
    unsafe {
        let texture = image as *mut ID3D11Texture2D;
        let mut desc: D3D11_TEXTURE2D_DESC = std::mem::zeroed();
        (*texture).GetDesc(&mut desc);
        Some((
            desc.Width,
            desc.Height,
            desc.Format,
            desc.SampleDesc.Count,
        ))
    }
}

/// Stage A の有効判定。`XR_STAGE_A` が `1` のときだけD3D単体の赤塗りを行う。
/// Stage A では ANGLE・GL・共有面に一切触らず、XR画像だけを赤くする。
/// ADR: 黒画面の切り分け専用の一時的な観測点であり、原因特定後に削除する。
fn stage_a_enabled() -> bool {
    std::env::var("XR_STAGE_A")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Stage A: XR画像へD3Dだけで赤を出す。`CopyResource` は使わない。
///
/// XR画像(90 TYPELESS)へ形式87(B8G8R8A8_UNORM)のRTVを作り、
/// `ClearRenderTargetView` で全面赤(1,0,0,1)に塗る。TYPELESS資源へのUNORM
/// ビューはDXGIの形式互換であり、形式不一致によるno-opは起きない。
/// `Flush` 後に呼び出し元がrelease・`xrEndFrame`へ進む。
/// ADR: STAGING読み戻しは一切行わない（device removal回避）。原因特定後に削除する。
fn stage_a_fill_red(
    ready: &mut CopyReady,
    dst: *mut ID3D11Texture2D,
    stats: &mut CopyStats,
    seq: u64,
    diagnosing: bool,
) {
    // SAFETY: 呼び出し元が保証する有効なXR画像・デバイス・contextである。
    // XRセッションはこのデバイスから作られており、XR画像は同一デバイス上の資源である。
    unsafe {
        let mut desc: D3D11_TEXTURE2D_DESC = std::mem::zeroed();
        (*dst).GetDesc(&mut desc);
        let view_dimension = if desc.SampleDesc.Count > 1 {
            D3D11_RTV_DIMENSION_TEXTURE2DMS
        } else {
            D3D11_RTV_DIMENSION_TEXTURE2D
        };
        // 共用体部は MipSlice=0 であり、ゼロ初期値のままでよい。
        let mut rtv_desc: D3D11_RENDER_TARGET_VIEW_DESC = std::mem::zeroed();
        rtv_desc.Format =
            winapi::shared::dxgiformat::DXGI_FORMAT_B8G8R8A8_UNORM;
        rtv_desc.ViewDimension = view_dimension;
        let mut rtv: *mut ID3D11RenderTargetView = ptr::null_mut();
        let create_hr = (*ready.d3d_device).CreateRenderTargetView(
            dst as *mut ID3D11Resource,
            &rtv_desc,
            &mut rtv,
        );
        if create_hr != S_OK || rtv.is_null() {
            stats.note_copy_failure();
            // 失敗分岐のログは初回3回と600フレームに1回に絞る（暴走防止）。
            if should_emit_fail(seq) {
                error!("[xr-copy] stage-a RTV creation failed: {create_hr:#x}");
                emit_json(serde_json::json!({
                    "event": "xr-copy-stage-a",
                    "seq": seq,
                    "ok": false,
                    "reason": "create_rtv_failed",
                    "create_hr": format!("{create_hr:#x}"),
                    "dst": {"width": desc.Width, "height": desc.Height,
                            "format": desc.Format, "samples": desc.SampleDesc.Count},
                }));
            }
            return;
        }
        // SAFETY: 直上で確保した+1参照をガードが放す。GLスレッドでのみ実行される。
        let _rtv_guard = ComPtr::from_raw(rtv);
        let begun = Instant::now();
        let red = [1.0f32, 0.0, 0.0, 1.0];
        (*ready.imm_context).ClearRenderTargetView(rtv, &red);
        (*ready.imm_context).Flush();
        let clear_ms = begun.elapsed().as_secs_f64() * 1000.0;
        // SAFETY: setupで保持した有効なデバイスに対する状態取得である。
        // XRデバイスに統一した後の所有関係を imperative に確認する。
        let removed = (*ready.d3d_device).GetDeviceRemovedReason();
        if removed != S_OK {
            stats.note_copy_failure();
            // 失敗分岐のログは初回3回と600フレームに1回に絞る（暴走防止）。
            if should_emit_fail(seq) {
                error!("[xr-copy] stage-a failed (device reason {removed:#x})");
            }
        }
        stats.record_copy(0.0, clear_ms);
        if diagnosing {
            emit_json(serde_json::json!({
                "event": "xr-copy-stage-a",
                "seq": seq,
                "ok": removed == S_OK,
                "create_hr": format!("{create_hr:#x}"),
                "removed_hr": format!("{removed:#x}"),
                "clear_ms": clear_ms,
                "dst": {"width": desc.Width, "height": desc.Height,
                        "format": desc.Format, "samples": desc.SampleDesc.Count},
                "rtv_format": rtv_desc.Format,
                "rtv_dimension": view_dimension,
            }));
        }
    }
}

/// E1実験の有効判定。既定では無効であり、E2（ページ内容の転送）が動く。
/// `XR_COPY_E1` が `on`・`1`・`e1` のときだけ旧来の赤塗りを行う。
/// `off` のときは診断fillの指定があっても赤塗りしない。
/// ADR: E2を既定にするため、診断fill（`XR_E1_FILL`）の明示指定があるときもE1系を動かす。
fn e1_enabled() -> bool {
    match std::env::var("XR_COPY_E1").as_deref() {
        Ok("on") | Ok("1") | Ok("e1") => true,
        Ok("off") => false,
        _ => e1_fill_d3d() || e1_fill_t2() || e1_fill_e3(),
    }
}

/// E1の赤塗り手段の切り替え。`XR_E1_FILL=d3d` のときだけD3Dで共有面を赤くする。
/// 既定（空）は従来通り、素面をbindしてGLで赤クリアする。
/// E1'（GLクリア→コピーが黒）が示した切断点が、GL→共有面の書き込みか
/// 共有面→XR画像のCopyResourceかを一段で切り分けるための一時的な綱である。
/// ADR: 原因特定後に削除する。
fn e1_fill_d3d() -> bool {
    std::env::var("XR_E1_FILL")
        .map(|v| v == "d3d")
        .unwrap_or(false)
}

/// T2の有効判定。`XR_E1_FILL=t2` のときだけ自前T(87)を赤くして写す。
/// 共有ハンドル由来物を経由せず、CopyResource(87→90)の合法性だけを見る。
/// ADR: 原因特定後に削除する。
fn e1_fill_t2() -> bool {
    std::env::var("XR_E1_FILL")
        .map(|v| v == "t2")
        .unwrap_or(false)
}

/// E3の有効判定。`XR_E1_FILL=e3` のときだけXRデバイス作成のTをANGLEに取り込んで描く。
/// T2で `CopyResource(T → XR画像)` の合法性は確定済みであり、E3ではGL描画が
/// ANGLE経由でTに届くかだけを切り分ける一段である。
/// ADR: 原因特定後に削除する。
fn e1_fill_e3() -> bool {
    std::env::var("XR_E1_FILL")
        .map(|v| v == "e3")
        .unwrap_or(false)
}

/// T1: D3Dだけで共有テクスチャを赤く塗る。GL・surfmanには一切触れない。
///
/// 共有テクスチャ（87 UNORM）へ形式87のRTVを作り、`ClearRenderTargetView` で
/// 全面赤(1,0,0,1)に塗る。Stage Aと同一手法であり、device removalの誘発はない。
/// この後の共通転送部が `CopyResource(共有面 → XR画像)` を行う。
/// viewportが赤になればCopyResourceは合法であり、黒の原因はGL側の書き込みにある。
/// 黒のままならCopyResource自体（形式87→90等）が無効である。
/// ADR: STAGING読み戻しは一切行わない（device removal回避）。原因特定後に削除する。
fn d3d_fill_shared_red(ready: &mut CopyReady, stats: &mut CopyStats, seq: u64, diagnosing: bool) {
    d3d_fill_red(ready, ready.shared_texture, "xr-copy-t1", stats, seq, diagnosing);
}

/// T2の中間テクスチャ。XRデバイス上で作った87の自前資源であり、共有ハンドルの
/// 由来物ではない。`CopyResource(T → XR画像)` の合法性だけを切り分ける。
/// ADR: T2診断専用の一時的な資源であり、原因特定後に削除する。
fn ensure_t2_texture(ready: &mut CopyReady) -> *mut ID3D11Texture2D {
    if !ready.t2_texture.is_null() {
        return ready.t2_texture;
    }
    // SAFETY: setupで保持した有効なデバイスに対する生成要求であり、出力先は有効である。
    // 寸法・形式は転送元と同一とし、転送先(90)との比較対象を形式87→90の一点に絞る。
    unsafe {
        let mut src_desc: D3D11_TEXTURE2D_DESC = std::mem::zeroed();
        (*ready.shared_texture).GetDesc(&mut src_desc);
        let t2_desc = D3D11_TEXTURE2D_DESC {
            Width: src_desc.Width,
            Height: src_desc.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: src_desc.Format,
            SampleDesc: src_desc.SampleDesc,
            Usage: winapi::um::d3d11::D3D11_USAGE_DEFAULT,
            BindFlags: winapi::um::d3d11::D3D11_BIND_RENDER_TARGET,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut t2: *mut ID3D11Texture2D = ptr::null_mut();
        let create_hr = (*ready.d3d_device).CreateTexture2D(&t2_desc, ptr::null(), &mut t2);
        if create_hr != S_OK || t2.is_null() {
            return ptr::null_mut();
        }
        ready.t2_texture = t2;
        t2
    }
}

/// 指定テクスチャへ形式87のRTVを作り赤く塗る。T1・T2の共通部である。
fn d3d_fill_red(
    ready: &mut CopyReady,
    target: *mut ID3D11Texture2D,
    event: &str,
    stats: &mut CopyStats,
    seq: u64,
    diagnosing: bool,
) {
    // SAFETY: 呼び出し元が保証する有効なテクスチャ・デバイス・contextである。
    // いずれもXRデバイス上の資源であり、同一デバイス上の操作である。
    unsafe {
        let mut desc: D3D11_TEXTURE2D_DESC = std::mem::zeroed();
        (*target).GetDesc(&mut desc);
        let view_dimension = if desc.SampleDesc.Count > 1 {
            D3D11_RTV_DIMENSION_TEXTURE2DMS
        } else {
            D3D11_RTV_DIMENSION_TEXTURE2D
        };
        let mut rtv_desc: D3D11_RENDER_TARGET_VIEW_DESC = std::mem::zeroed();
        rtv_desc.Format = winapi::shared::dxgiformat::DXGI_FORMAT_B8G8R8A8_UNORM;
        rtv_desc.ViewDimension = view_dimension;
        let mut rtv: *mut ID3D11RenderTargetView = ptr::null_mut();
        let create_hr = (*ready.d3d_device).CreateRenderTargetView(
            target as *mut ID3D11Resource,
            &rtv_desc,
            &mut rtv,
        );
        if create_hr != S_OK || rtv.is_null() {
            stats.note_copy_failure();
            if should_emit_fail(seq) {
                error!("[xr-copy] {event} RTV creation failed: {create_hr:#x}");
                emit_json(serde_json::json!({
                    "event": event,
                    "seq": seq,
                    "ok": false,
                    "reason": "create_rtv_failed",
                    "create_hr": format!("{create_hr:#x}"),
                }));
            }
            return;
        }
        // SAFETY: 直上で確保した+1参照をガードが放す。GLスレッドでのみ実行される。
        let _rtv_guard = ComPtr::from_raw(rtv);
        let red = [1.0f32, 0.0, 0.0, 1.0];
        (*ready.imm_context).ClearRenderTargetView(rtv, &red);
        (*ready.imm_context).Flush();
        // SAFETY: setupで保持した有効なデバイスに対する状態取得である。
        let removed = (*ready.d3d_device).GetDeviceRemovedReason();
        if removed != S_OK {
            stats.note_copy_failure();
            if should_emit_fail(seq) {
                error!("[xr-copy] {event} fill failed (device reason {removed:#x})");
            }
        }
        if diagnosing {
            emit_json(serde_json::json!({
                "event": event,
                "seq": seq,
                "ok": removed == S_OK,
                "create_hr": format!("{create_hr:#x}"),
                "removed_hr": format!("{removed:#x}"),
                "dst": {"width": desc.Width, "height": desc.Height,
                        "format": desc.Format, "samples": desc.SampleDesc.Count},
            }));
        }
    }
}

/// E3の裏付け。XRデバイス上の自前テクスチャTと、ANGLEに取り込んだ描画先Sである。
/// T2で `CopyResource(T → XR画像)` の合法性は確定済みであり、E3ではGL描画が
/// ANGLE経由でTに届くかだけを切り分ける。
/// ADR: E3診断専用の一時的な裏付けであり、原因特定後に削除する。
struct E3State {
    /// XRデバイス上のT(87, MISC_SHARED)。+1参照。GLスレッドでのみ操作する。
    t: *mut ID3D11Texture2D,
    /// ANGLEに取り込んだ描画先。dance中だけNoneになる。
    surface: Option<SurfmanSurface>,
}

/// E3裏付けを破棄する。danceの成否にかかわらず、所在が分かる資源は解放する。
/// Sの所在が不明の場合（unbind失敗時）はANGLE側に残るが、E3診断の一時経路であり
/// 終了時に回収される（E1の復旧不能経路と同一方針）。
fn teardown_e3(
    device: &SurfmanDevice,
    context: &mut SurfmanContext,
    ready: &mut CopyReady,
) {
    if let Some(mut e3) = ready.e3.take() {
        if let Some(mut surface) = e3.surface.take() {
            let _ = device.destroy_surface(context, &mut surface);
        }
        // SAFETY: E3で取得した+1参照をここで放す。GLスレッドでのみ実行される。
        unsafe {
            if !e3.t.is_null() {
                drop(ComPtr::from_raw(e3.t));
            }
        }
    }
}

/// E3裏付けを遅延生成する。既存なら何もしない。
///
/// XRデバイス上にT(87, MISC_SHARED)を作り、その共有ハンドルをANGLEデバイス上で
/// 開き直して `create_surface_from_texture` で取り込んだSを作る。TへのRTVクリアは
/// 行わない（GL描画の到達だけを見るため、Tの初期内容は問わない）。
/// ADR: E3診断専用の一時的な裏付けであり、原因特定後に削除する。
fn ensure_e3(
    contexts: &mut dyn webxr_api::GLContexts<SurfmanGL>,
    context_id: ContextId,
    device: &SurfmanDevice,
    ready: &mut CopyReady,
) -> Result<(), String> {
    if ready.e3.is_some() {
        return Ok(());
    }
    // TをXRデバイス上に作る。形式・寸法は転送元と同一とする。
    let (t, t_handle) = unsafe {
        let mut src_desc: D3D11_TEXTURE2D_DESC = std::mem::zeroed();
        (*ready.shared_texture).GetDesc(&mut src_desc);
        let t_desc = D3D11_TEXTURE2D_DESC {
            Width: src_desc.Width,
            Height: src_desc.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: src_desc.Format,
            SampleDesc: src_desc.SampleDesc,
            Usage: winapi::um::d3d11::D3D11_USAGE_DEFAULT,
            BindFlags: winapi::um::d3d11::D3D11_BIND_RENDER_TARGET,
            CPUAccessFlags: 0,
            MiscFlags: winapi::um::d3d11::D3D11_RESOURCE_MISC_SHARED,
        };
        let mut t: *mut ID3D11Texture2D = ptr::null_mut();
        let create_hr = (*ready.d3d_device).CreateTexture2D(&t_desc, ptr::null(), &mut t);
        if create_hr != S_OK || t.is_null() {
            return Err(format!("E3 CreateTexture2D failed: {create_hr:#x}"));
        }
        // 共有ハンドルを取得する。
        // SAFETY: 生成したばかりの有効なテクスチャに対する問い合わせである。
        let mut res: *mut IDXGIResource = ptr::null_mut();
        let qi_hr = (*t).QueryInterface(
            &IDXGIResource::uuidof(),
            &mut res as *mut *mut IDXGIResource as *mut *mut c_void,
        );
        if qi_hr != S_OK || res.is_null() {
            drop(ComPtr::from_raw(t));
            return Err(format!("E3 QueryInterface IDXGIResource failed: {qi_hr:#x}"));
        }
        let res = ComPtr::from_raw(res);
        let mut handle: *mut c_void = ptr::null_mut();
        let handle_hr = res.GetSharedHandle(&mut handle);
        if handle_hr != S_OK || handle.is_null() {
            drop(ComPtr::from_raw(t));
            return Err(format!("E3 GetSharedHandle failed: {handle_hr:#x}"));
        }
        (t, handle)
    };
    // ハンドルをANGLEデバイス上で開く。
    let context = contexts
        .context(context_id)
        .ok_or_else(|| "E3: no surfman context".to_owned())?;
    let size = euclid::Size2D::new(ready.width, ready.height);
    // SAFETY: native_device()は+1参照で返すためComPtrで引き継ぎ、開いたテクスチャ(+1)ごと
    // create_surface_from_textureへ所有権を渡す。Err時は被呼出側で破棄される。
    unsafe {
        let native = device.native_device();
        if native.d3d11_device.is_null() {
            drop(ComPtr::from_raw(t));
            return Err("E3: ANGLE d3d11_device is null".into());
        }
        let angle_device = ComPtr::from_raw(native.d3d11_device);
        let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
        let open_hr = (*angle_device).OpenSharedResource(
            t_handle,
            &ID3D11Texture2D::uuidof(),
            &mut tex as *mut *mut ID3D11Texture2D as *mut *mut c_void,
        );
        if open_hr != S_OK || tex.is_null() {
            drop(ComPtr::from_raw(t));
            return Err(format!("E3 OpenSharedResource on ANGLE failed: {open_hr:#x}"));
        }
        match device.create_surface_from_texture(context, &size, ComPtr::from_raw(tex)) {
            Ok(surface) => {
                emit_json(serde_json::json!({
                    "event": "xr-copy-e3-init",
                    "ok": true,
                    "t_handle": format!("{:#x}", t_handle as usize),
                    "size": {"width": ready.width, "height": ready.height},
                }));
                ready.e3 = Some(E3State {
                    t,
                    surface: Some(surface),
                });
                Ok(())
            },
            Err(e) => {
                drop(ComPtr::from_raw(t));
                Err(format!("E3 create_surface_from_texture failed: {e:?}"))
            },
        }
    }
}

/// E3: 取り込み面Sをbindして既定FBへ赤クリアする。ページ内容は無視する。
///
/// E1の素面danceと同一手順だが、SにSurfaceTextureは無いため作り直しは行わない。
/// Sは呼び出し後にE3Stateへ戻す。unbind失敗時はSの所在が不明になるため、
/// 呼び出し元がteardown_e3でTごと破棄し、次フレーム作り直す。
/// ADR: E3診断専用であり、D3DのSTAGING読み戻しは一切行わない。
fn e3_bind_clear_red(
    contexts: &mut dyn webxr_api::GLContexts<SurfmanGL>,
    context_id: ContextId,
    device: &SurfmanDevice,
    e3: &mut E3State,
    width: i32,
    height: i32,
) -> Result<serde_json::Value, String> {
    let mut detail = serde_json::Map::new();
    let surface = e3.surface.take().ok_or_else(|| "E3: no surface".to_owned())?;
    // 段階1: 既存の束縛面を退避する。WebGLコンテキストは自前の面を持っている。
    let orig_surface = {
        let context = contexts
            .context(context_id)
            .ok_or_else(|| "no surfman context for E3".to_owned())?;
        match device.unbind_surface_from_context(context) {
            Ok(surface) => {
                detail.insert(
                    "unbind_orig_ok".into(),
                    serde_json::Value::Bool(surface.is_some()),
                );
                surface
            },
            Err(e) => {
                e3.surface = Some(surface);
                return Err(format!("unbind orig surface failed: {e:?}"));
            },
        }
    };
    // 段階2: Sをbindしてカレントにする。
    {
        let context = contexts
            .context(context_id)
            .ok_or_else(|| "no surfman context for E3 bind".to_owned())?;
        if let Err((e, surface)) = device.bind_surface_to_context(context, surface) {
            let _ = device.destroy_surface(context, &mut { surface });
            if let Some(orig) = orig_surface {
                let _ = device.bind_surface_to_context(context, orig);
                let _ = device.make_context_current(context);
            }
            return Err(format!("bind E3 surface failed: {e:?}"));
        }
        if let Err(e) = device.make_context_current(context) {
            // Sの所在はbind済みだが、後続のunbindで回収を試みる。
            if let Some(orig) = orig_surface {
                let _ = device.bind_surface_to_context(context, orig);
                let _ = device.make_context_current(context);
            }
            return Err(format!("make_current for E3 surface failed: {e:?}"));
        }
        detail.insert("bind_e3_ok".into(), serde_json::Value::Bool(true));
    }
    // 段階3: 既定フレームバッファへ赤クリアする。E1とE3の共通部である。
    match clear_default_red(contexts, context_id, width, height) {
        Ok((gl_error, fbo_status, sampled)) => {
            detail.insert("gl_error".into(), serde_json::json!(format!("{gl_error:#x}")));
            detail.insert(
                "fbo_status".into(),
                serde_json::json!(format!("{fbo_status:#x}")),
            );
            detail.insert(
                "sampled_rgba".into(),
                serde_json::json!([sampled[0], sampled[1], sampled[2]]),
            );
        },
        Err(reason) => {
            // Sを外して戻し、既存面を復帰させる。
            if let Some(context) = contexts.context(context_id) {
                if let Ok(Some(s)) = device.unbind_surface_from_context(context) {
                    e3.surface = Some(s);
                }
                if let Some(orig) = orig_surface {
                    let _ = device.bind_surface_to_context(context, orig);
                    let _ = device.make_context_current(context);
                } else {
                    // Sの所在が不明なためteardown対象として空のままにする。
                }
            }
            return Err(reason);
        },
    }
    // 段階4: Sを外してE3Stateへ戻し、既存面を戻す。
    {
        let context = contexts
            .context(context_id)
            .ok_or_else(|| "no surfman context for E3 unbind".to_owned())?;
        match device.unbind_surface_from_context(context) {
            Ok(Some(s)) => {
                e3.surface = Some(s);
            },
            Ok(None) => {
                if let Some(orig) = orig_surface {
                    let _ = device.bind_surface_to_context(context, orig);
                    let _ = device.make_context_current(context);
                }
                return Err("E3: no bound surface after clear".into());
            },
            Err(e) => {
                if let Some(orig) = orig_surface {
                    let _ = device.bind_surface_to_context(context, orig);
                    let _ = device.make_context_current(context);
                }
                return Err(format!("unbind E3 surface failed: {e:?}"));
            },
        }
    }
    if let Some(orig) = orig_surface {
        let context = contexts
            .context(context_id)
            .ok_or_else(|| "no surfman context for E3 restore".to_owned())?;
        match device.bind_surface_to_context(context, orig) {
            Ok(()) => {
                let _ = device.make_context_current(context);
                detail.insert("restore_orig_ok".into(), serde_json::Value::Bool(true));
            },
            Err((e, mut surface)) => {
                let _ = device.destroy_surface(context, &mut surface);
                return Err(format!("restore orig surface failed: {e:?}"));
            },
        }
    } else {
        detail.insert("restore_orig_ok".into(), serde_json::Value::Bool(false));
    }
    Ok(serde_json::Value::Object(detail))
}
/// 既定フレームバッファ（bind中の面）へ赤クリアする。ANGLEのpbufferはNoneが既定である。
/// 直前のページ描画が残したGLエラーを先に捨て、今回のclearの成否だけを見る。
/// さらに既定フレームバッファから3点（左端・中央・右端）を読み戻し、赤が載ったかをGL側で確かめる。
/// 戻り値は (clear直後と読み戻し後を合成したGLエラー, FBO状態, 採取画素)。
/// ADR: GL側のreadPixelsであり、D3DのSTAGING/Mapは使わない（device removal回避）。
/// E1とE3の共通部である。
fn clear_default_red(
    contexts: &mut dyn webxr_api::GLContexts<SurfmanGL>,
    context_id: ContextId,
    width: i32,
    height: i32,
) -> Result<(u32, u32, [[u8; 4]; 3]), String> {
    let Some(gl) = contexts.bindings(context_id) else {
        return Err("no GL bindings for default-framebuffer clear".into());
    };
    // SAFETY: GLスレッド上の有効なコンテキストに対する描画であり、FBOの結び替えは元に戻す。
    unsafe {
        // 残留エラーを捨てる（上限付き）。
        for _ in 0..16 {
            if gl.get_error() == glow::NO_ERROR {
                break;
            }
        }
        let draw_fbo = gl.get_parameter_framebuffer(glow::DRAW_FRAMEBUFFER_BINDING);
        let read_fbo = gl.get_parameter_framebuffer(glow::READ_FRAMEBUFFER_BINDING);
        // 既定フレームバッファ（素面）へ向ける。
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        // 全面を確実に塗るため、視域を素面全体にしてシザーを外す。
        let mut prev_viewport = [0; 4];
        gl.get_parameter_i32_slice(glow::VIEWPORT, &mut prev_viewport);
        let scissor_enabled = gl.is_enabled(glow::SCISSOR_TEST);
        gl.disable(glow::SCISSOR_TEST);
        gl.viewport(0, 0, width, height);
        gl.clear_color(1.0, 0.0, 0.0, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.finish();
        let error = gl.get_error();
        // 既定フレームバッファから3点を読み戻す（左端・中央・右端）。
        let y = height / 2;
        let mut sampled = [[0u8; 4]; 3];
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
        for (i, x) in [2, width / 2, width - 3].iter().enumerate() {
            let mut buf = [0u8; 4];
            gl.read_pixels(
                *x,
                y,
                1,
                1,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut buf[..])),
            );
            sampled[i] = buf;
        }
        let read_error = gl.get_error();
        // 状態を戻す。FBO名は文脈全体で有効なため単純に戻してよい。
        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, draw_fbo);
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, read_fbo);
        gl.viewport(
            prev_viewport[0],
            prev_viewport[1],
            prev_viewport[2],
            prev_viewport[3],
        );
        if scissor_enabled {
            gl.enable(glow::SCISSOR_TEST);
        }
        // clear直後のエラーと読み戻し後のエラーを合成する。
        let combined = if error != glow::NO_ERROR {
            error
        } else {
            read_error
        };
        Ok((combined, status, sampled))
    }
}

/// E2-blitの上下反転切り替え。`XR_BLIT_FLIP` が `1`・`on`・`flip` のときだけY反転する。
/// 既定は等倍（非反転）であり、どちらを使ったかは転送ログに残す。
/// ADR: GL原点（左下）とD3D行順（上端が行0）の差異をblit矩形で吸収する綱である。
fn blit_flip_enabled() -> bool {
    match std::env::var("XR_BLIT_FLIP").as_deref() {
        Ok("1") | Ok("on") | Ok("flip") | Ok("true") => true,
        _ => false,
    }
}

/// ページ描画先（`page_texture`）の内容を既定フレームバッファへ `glBlitFramebuffer` で移す。
/// 呼び出し時点でE3の面Sがbind済み・カレントであることを前提とする。
/// 戻り値は（blit前後の合成GLエラー、READ側FBO状態、DRAW側FBO状態、Y反転の有無、blit後の既定FB採取画素）。
/// 診断のGL readPixelsであり、D3DのSTAGING/Mapは使わない。
/// ADR: FBO経由の描き込み（E1'/E2旧経路）が死角のため、bindした面へのblitで繋ぐ一段である。
fn blit_page_to_default(
    contexts: &mut dyn webxr_api::GLContexts<SurfmanGL>,
    context_id: ContextId,
    ready: &CopyReady,
) -> Result<(u32, u32, u32, bool, [[u8; 4]; 3]), String> {
    let Some(gl) = contexts.bindings(context_id) else {
        return Err("no GL bindings for blit".into());
    };
    let Some(page) = ready.page_texture else {
        return Err("no page_texture for blit".into());
    };
    let (w, h) = (ready.width, ready.height);
    let flip = blit_flip_enabled();
    // SAFETY: GLスレッド上の有効なコンテキストに対するblitであり、FBOの結び替えは元に戻し、
    // 診断用FBOは都度捨てる。既存のページ描画内容は変えない。
    unsafe {
        for _ in 0..16 {
            if gl.get_error() == glow::NO_ERROR {
                break;
            }
        }
        let draw_fbo = gl.get_parameter_framebuffer(glow::DRAW_FRAMEBUFFER_BINDING);
        let read_fbo = gl.get_parameter_framebuffer(glow::READ_FRAMEBUFFER_BINDING);
        let mut prev_viewport = [0; 4];
        gl.get_parameter_i32_slice(glow::VIEWPORT, &mut prev_viewport);
        let scissor_enabled = gl.is_enabled(glow::SCISSOR_TEST);
        gl.disable(glow::SCISSOR_TEST);
        let read_tmp = match gl.create_framebuffer() {
            Ok(fbo) => fbo,
            Err(e) => return Err(format!("blit create READ FBO failed: {e:?}")),
        };
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(read_tmp));
        gl.framebuffer_texture_2d(
            glow::READ_FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            ready.page_target,
            Some(page),
            0,
        );
        let read_status = gl.check_framebuffer_status(glow::READ_FRAMEBUFFER);
        if read_status != glow::FRAMEBUFFER_COMPLETE {
            let err = gl.get_error();
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, draw_fbo);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, read_fbo);
            gl.delete_framebuffer(read_tmp);
            if scissor_enabled {
                gl.enable(glow::SCISSOR_TEST);
            }
            return Err(format!("blit READ FBO incomplete: {read_status:#x} err {err:#x}"));
        }
        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, None);
        let draw_status = gl.check_framebuffer_status(glow::DRAW_FRAMEBUFFER);
        if draw_status != glow::FRAMEBUFFER_COMPLETE {
            let err = gl.get_error();
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, draw_fbo);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, read_fbo);
            gl.delete_framebuffer(read_tmp);
            if scissor_enabled {
                gl.enable(glow::SCISSOR_TEST);
            }
            return Err(format!("blit DRAW FBO incomplete: {draw_status:#x} err {err:#x}"));
        }
        if flip {
            gl.blit_framebuffer(
                0,
                0,
                w,
                h,
                0,
                h,
                w,
                0,
                glow::COLOR_BUFFER_BIT,
                glow::NEAREST,
            );
        } else {
            gl.blit_framebuffer(
                0,
                0,
                w,
                h,
                0,
                0,
                w,
                h,
                glow::COLOR_BUFFER_BIT,
                glow::NEAREST,
            );
        }
        let blit_error = gl.get_error();
        gl.finish();
        let finish_error = gl.get_error();
        // blit後の既定FBから3点を読み戻す（左端・中央・右端）。
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
        let y = h / 2;
        let mut sampled = [[0u8; 4]; 3];
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
        for (i, x) in [2, w / 2, w - 3].iter().enumerate() {
            let mut buf = [0u8; 4];
            gl.read_pixels(
                *x,
                y,
                1,
                1,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut buf[..])),
            );
            sampled[i] = buf;
        }
        let read_error = gl.get_error();
        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, draw_fbo);
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, read_fbo);
        gl.delete_framebuffer(read_tmp);
        gl.viewport(
            prev_viewport[0],
            prev_viewport[1],
            prev_viewport[2],
            prev_viewport[3],
        );
        if scissor_enabled {
            gl.enable(glow::SCISSOR_TEST);
        }
        let combined = if blit_error != glow::NO_ERROR {
            blit_error
        } else if finish_error != glow::NO_ERROR {
            finish_error
        } else {
            read_error
        };
        Ok((combined, read_status, draw_status, flip, sampled))
    }
}

/// E2-blit: ページ確定→E3面へbind→blit→元へ復帰の一連を行う。
/// E3裏付けが無ければここで遅延生成する。失敗時は可能な範囲で既存面を戻し、
/// 呼び出し元がstale提出で継続できるようにする。panicさせない。
/// ADR: E2既定転送であり、D3DのSTAGING読み戻しは一切行わない。
fn e2_blit_page_to_e3(
    contexts: &mut dyn webxr_api::GLContexts<SurfmanGL>,
    context_id: ContextId,
    device: &SurfmanDevice,
    ready: &mut CopyReady,
) -> Result<serde_json::Value, String> {
    let mut detail = serde_json::Map::new();
    // 段階0: ページ描画を確定させ、ページ面の画素を採取する（bind前でも読める）。
    let finish_ms = match e2_finish_page(contexts, context_id) {
        Ok(ms) => {
            detail.insert("finish_ms".into(), serde_json::json!(ms));
            ms
        },
        Err(reason) => return Err(reason),
    };
    let _ = finish_ms;
    if let Some(gl) = contexts.bindings(context_id) {
        let diag = diag_read_gl(&gl, ready);
        detail.insert("page_sample".into(), diag.value);
    }
    // E3裏付けを遅延生成する（MISC_SHAREDのT＋取り込み面S、mutexなし）。
    if let Err(reason) = ensure_e3(contexts, context_id, device, ready) {
        return Err(reason);
    }
    let (w, h) = (ready.width, ready.height);
    // 段階1: 既存の束縛面を退避する。WebGLコンテキストは自前の面を持っている。
    let orig_surface = {
        let context = contexts
            .context(context_id)
            .ok_or_else(|| "no surfman context for E2-blit".to_owned())?;
        match device.unbind_surface_from_context(context) {
            Ok(surface) => {
                detail.insert(
                    "unbind_orig_ok".into(),
                    serde_json::Value::Bool(surface.is_some()),
                );
                surface
            },
            Err(e) => return Err(format!("unbind orig surface failed: {e:?}")),
        }
    };
    // 段階2: E3のSをbindしてカレントにする。
    {
        let e3 = ready
            .e3
            .as_mut()
            .ok_or_else(|| "no E3 backing for blit".to_owned())?;
        let surface = e3.surface.take().ok_or_else(|| "E2-blit: no surface".to_owned())?;
        let context = contexts
            .context(context_id)
            .ok_or_else(|| "no surfman context for E2-blit bind".to_owned())?;
        if let Err((e, surface)) = device.bind_surface_to_context(context, surface) {
            let _ = device.destroy_surface(context, &mut { surface });
            if let Some(orig) = orig_surface {
                let _ = device.bind_surface_to_context(context, orig);
                let _ = device.make_context_current(context);
            }
            return Err(format!("bind E3 surface failed: {e:?}"));
        }
        if let Err(e) = device.make_context_current(context) {
            if let Some(orig) = orig_surface {
                let _ = device.bind_surface_to_context(context, orig);
                let _ = device.make_context_current(context);
            }
            return Err(format!("make_current for E3 surface failed: {e:?}"));
        }
        detail.insert("bind_e3_ok".into(), serde_json::Value::Bool(true));
    }
    // 段階3: ページ面から既定FB（E3のT）へblitする。
    match blit_page_to_default(contexts, context_id, ready) {
        Ok((gl_error, read_status, draw_status, flip, sampled)) => {
            detail.insert("blit_ok".into(), serde_json::Value::Bool(true));
            detail.insert("blit_flip_y".into(), serde_json::Value::Bool(flip));
            detail.insert(
                "blit_gl_error".into(),
                serde_json::json!(format!("{gl_error:#x}")),
            );
            detail.insert(
                "blit_read_status".into(),
                serde_json::json!(format!("{read_status:#x}")),
            );
            detail.insert(
                "blit_draw_status".into(),
                serde_json::json!(format!("{draw_status:#x}")),
            );
            detail.insert(
                "blit_sampled_rgba".into(),
                serde_json::json!([sampled[0], sampled[1], sampled[2]]),
            );
        },
        Err(reason) => {
            if let Some(context) = contexts.context(context_id) {
                if let Ok(Some(s)) = device.unbind_surface_from_context(context) {
                    if let Some(e3) = ready.e3.as_mut() {
                        e3.surface = Some(s);
                    }
                }
                if let Some(orig) = orig_surface {
                    let _ = device.bind_surface_to_context(context, orig);
                    let _ = device.make_context_current(context);
                } else {
                    // Sの所在が不明なためteardown対象として空のままにする。
                }
            }
            return Err(reason);
        },
    }
    // 段階4: Sを外してE3Stateへ戻し、既存面を戻す。
    {
        let context = contexts
            .context(context_id)
            .ok_or_else(|| "no surfman context for E2-blit unbind".to_owned())?;
        match device.unbind_surface_from_context(context) {
            Ok(Some(s)) => {
                if let Some(e3) = ready.e3.as_mut() {
                    e3.surface = Some(s);
                }
            },
            Ok(None) => {
                if let Some(orig) = orig_surface {
                    let _ = device.bind_surface_to_context(context, orig);
                    let _ = device.make_context_current(context);
                }
                return Err("E2-blit: no bound surface after blit".into());
            },
            Err(e) => {
                if let Some(orig) = orig_surface {
                    let _ = device.bind_surface_to_context(context, orig);
                    let _ = device.make_context_current(context);
                }
                return Err(format!("unbind E2-blit surface failed: {e:?}"));
            },
        }
    }
    if let Some(orig) = orig_surface {
        let context = contexts
            .context(context_id)
            .ok_or_else(|| "no surfman context for E2-blit restore".to_owned())?;
        match device.bind_surface_to_context(context, orig) {
            Ok(()) => {
                let _ = device.make_context_current(context);
                detail.insert("restore_orig_ok".into(), serde_json::Value::Bool(true));
            },
            Err((e, mut surface)) => {
                let _ = device.destroy_surface(context, &mut surface);
                return Err(format!("restore orig surface failed: {e:?}"));
            },
        }
    } else {
        detail.insert("restore_orig_ok".into(), serde_json::Value::Bool(false));
    }
    Ok(serde_json::Value::Object(detail))
}

/// E1: 描画面（E2では取り込み面S）の素面をWebGLコンテキストにbindして赤で塗る。
///
/// 従来は `SurfaceTexture` のGLテクスチャへFBO経由で描いていたが、surfman 0.14の
/// ANGLE実装ではそのテクスチャは読み取り専用（`BindTexImage` のサンプリング用）であり、
/// 描いてもD3D共有面へ反映されない。素面をbindした既定フレームバッファへ描けば
/// D3D面そのものが描画先になるはずであり、その可否を目視で確かめる。
/// 手順はsurfman 0.14の `tests.rs` と `angle/device.rs` に従う:
/// `unbind(既存面)` → `destroy_surface_texture`で素面を取り出す →
/// `bind_surface_to_context` → `make_context_current` →
/// 既定フレームバッファへ赤クリア → `glFinish` →
/// `unbind` → `create_surface_texture`で作り直し → 既存面をbindし直す。
/// WebGLコンテキストは既に自前の面を持っているため、一時的に退避する。
/// 失敗時は可能な範囲で既存面を戻し、呼び出し元がstale提出で継続できるようにする。
/// ADR: E1診断専用であり、D3DのSTAGING読み戻しは一切行わない（device removal回避）。
fn e1_bind_clear_red(
    contexts: &mut dyn webxr_api::GLContexts<SurfmanGL>,
    context_id: ContextId,
    device: &SurfmanDevice,
    ready: &mut CopyReady,
) -> Result<serde_json::Value, String> {
    let mut detail = serde_json::Map::new();
    // 段階1: 既存の束縛面を退避する。WebGLコンテキストは自前の面を持っている。
    let orig_surface = {
        let context = contexts
            .context(context_id)
            .ok_or_else(|| "no surfman context for E1".to_owned())?;
        match device.unbind_surface_from_context(context) {
            Ok(surface) => {
                detail.insert(
                    "unbind_orig_ok".into(),
                    serde_json::Value::Bool(surface.is_some()),
                );
                surface
            },
            Err(e) => {
                return Err(format!("unbind orig surface failed: {e:?}"));
            },
        }
    };
    // 段階2: SurfaceTextureを壊して素面を取り出す。
    let surface_texture = ready
        .surface_texture
        .take()
        .ok_or_else(|| "E1: no surface_texture".to_owned())?;
    let xr_surface = {
        let context = contexts
            .context(context_id)
            .ok_or_else(|| "no surfman context for E1 destroy".to_owned())?;
        match device.destroy_surface_texture(context, surface_texture) {
            Ok(surface) => {
                detail.insert("destroy_texture_ok".into(), serde_json::Value::Bool(true));
                surface
            },
            Err((e, surface_texture)) => {
                // 復帰不能なままDropするとpanicするためリークし、既存面を戻す。
                std::mem::forget(surface_texture);
                if let Some(orig) = orig_surface {
                    if let Some(context) = contexts.context(context_id) {
                        let _ = device.bind_surface_to_context(context, orig);
                        let _ = device.make_context_current(context);
                    } else {
                        std::mem::forget(orig);
                    }
                }
                return Err(format!("destroy_surface_texture failed: {e:?}"));
            },
        }
    };
    // 段階3: 素面をbindしてカレントにする。
    {
        let context = contexts
            .context(context_id)
            .ok_or_else(|| "no surfman context for E1 bind".to_owned())?;
        if let Err((e, surface)) = device.bind_surface_to_context(context, xr_surface) {
            // bind失敗時は素面を作り直して退避面を戻す。
            let _ = device.destroy_surface(context, &mut { surface });
            if let Some(orig) = orig_surface {
                let _ = device.bind_surface_to_context(context, orig);
                let _ = device.make_context_current(context);
            }
            return Err(format!("bind xr surface failed: {e:?}"));
        }
        if let Err(e) = device.make_context_current(context) {
            return Err(format!("make_current for xr surface failed: {e:?}"));
        }
        detail.insert("bind_xr_ok".into(), serde_json::Value::Bool(true));
    }
    // 段階4: 既定フレームバッファへ赤クリアする。E1とE3の共通部である。
    let (gl_error, fbo_status, sampled) =
        match clear_default_red(contexts, context_id, ready.width, ready.height) {
            Ok(v) => v,
            Err(reason) => return Err(reason),
        };
    detail.insert(
        "clear_fbo_is_default".into(),
        serde_json::Value::Bool(true),
    );
    detail.insert(
        "gl_error".into(),
        serde_json::json!(format!("{gl_error:#x}")),
    );
    detail.insert(
        "viewport_applied".into(),
        serde_json::Value::Bool(true),
    );
    detail.insert(
        "fbo_status".into(),
        serde_json::json!(format!("{fbo_status:#x}")),
    );
    detail.insert(
        "sampled_rgba".into(),
        serde_json::json!([sampled[0], sampled[1], sampled[2]]),
    );
    // 段階5: 素面を外してSurfaceTextureを作り直し、既存面を戻す。
    let xr_surface = {
        let context = contexts
            .context(context_id)
            .ok_or_else(|| "no surfman context for E1 unbind".to_owned())?;
        match device.unbind_surface_from_context(context) {
            Ok(Some(surface)) => surface,
            Ok(None) => {
                if let Some(orig) = orig_surface {
                    let _ = device.bind_surface_to_context(context, orig);
                    let _ = device.make_context_current(context);
                }
                return Err("E1: no bound xr surface after clear".into());
            },
            Err(e) => {
                if let Some(orig) = orig_surface {
                    // 素面の所在が不明なため復旧不能。既存面だけ戻してpanicを避ける。
                    let _ = device.bind_surface_to_context(context, orig);
                    let _ = device.make_context_current(context);
                }
                return Err(format!("unbind xr surface failed: {e:?}"));
            },
        }
    };
    {
        let context = contexts
            .context(context_id)
            .ok_or_else(|| "no surfman context for E1 recreate".to_owned())?;
        match device.create_surface_texture(context, xr_surface) {
            Ok(new_texture) => {
                let gl_object = device.surface_texture_object(&new_texture);
                match gl_object {
                    Some(tex) => {
                        ready.gl_texture = tex;
                    },
                    None => {
                        // テクスチャ生成自体は成功したため保持し、GL名だけ更新できない。
                        ready.surface_texture = Some(new_texture);
                        if let Some(orig) = orig_surface {
                            let _ = device.bind_surface_to_context(context, orig);
                            let _ = device.make_context_current(context);
                        }
                        return Err("E1: recreated texture has no GL object".into());
                    },
                }
                ready.surface_texture = Some(new_texture);
                detail.insert("recreate_ok".into(), serde_json::Value::Bool(true));
            },
            Err((e, mut surface)) => {
                let _ = device.destroy_surface(context, &mut surface);
                if let Some(orig) = orig_surface {
                    let _ = device.bind_surface_to_context(context, orig);
                    let _ = device.make_context_current(context);
                }
                return Err(format!("recreate surface_texture failed: {e:?}"));
            },
        }
    }
    if let Some(orig) = orig_surface {
        let context = contexts
            .context(context_id)
            .ok_or_else(|| "no surfman context for E1 restore".to_owned())?;
        match device.bind_surface_to_context(context, orig) {
            Ok(()) => {
                let _ = device.make_context_current(context);
                detail.insert("restore_orig_ok".into(), serde_json::Value::Bool(true));
            },
            Err((e, mut surface)) => {
                let _ = device.destroy_surface(context, &mut surface);
                return Err(format!("restore orig surface failed: {e:?}"));
            },
        }
    } else {
        detail.insert("restore_orig_ok".into(), serde_json::Value::Bool(false));
    }
    Ok(serde_json::Value::Object(detail))
}

/// E2（既定動作）: ページが描いたTの内容をXR画像へ転送する。CPU読み戻しは行わない。
/// E1系（診断）: 転送直前に赤クリアした面の内容を転送する。
///
/// ページ描画の確定は `glFinish` で行い、その所要時間を統計に記録する。
/// E2では赤塗りをしない。ページのFBO描画がTへ届くことが前提であり、
/// 届かない場合は `XR_E1_FILL=e3` のE3診断で切り分ける。
/// ADR: E3で `T → XR画像` の転送とGL→Tの到達は確定済みであり、E2はその常用化である。
fn e2_finish_page(
    contexts: &mut dyn webxr_api::GLContexts<SurfmanGL>,
    context_id: ContextId,
) -> Result<f64, String> {
    let Some(gl) = contexts.bindings(context_id) else {
        return Err("no GL bindings for page-finish".into());
    };
    // SAFETY: GLスレッド上の有効なコンテキストに対する確定待ちであり、描画内容は変えない。
    unsafe {
        // 残留エラーを捨て、今回のfinishの成否だけを見る（上限付き）。
        for _ in 0..16 {
            if gl.get_error() == glow::NO_ERROR {
                break;
            }
        }
        let begun = Instant::now();
        gl.finish();
        Ok(begun.elapsed().as_secs_f64() * 1000.0)
    }
}

/// E1では転送直前に素面へ赤クリアし、D3D面そのものへの描画が共有面へ載るかを見る。
/// T1（`XR_E1_FILL=d3d`）ではD3Dで共有面を赤くし、CopyResource自体の合法性を切り分ける。
/// `gl.finish`で描画を確定させてから`CopyResource`（MSAA時は`ResolveSubresource`）し、
/// `Flush`する。失敗時は統計に記録するのみでpanicしない。提出はstale画像のまま継続する。
/// 診断を行った場合はその通番を返し、呼び出し元が提出内容と対応づけられるようにする。
/// ADR: E1期間中はD3DのSTAGING読み戻しを一切行わない（device removal回避のため）。
pub(super) fn copy_to_swapchain(
    contexts: &mut dyn webxr_api::GLContexts<SurfmanGL>,
    context_id: ContextId,
    ready: &mut CopyReady,
    dst: <Backend as Graphics>::SwapchainImage,
    stats: &mut CopyStats,
) -> Option<u64> {
    if dst.is_null() {
        stats.note_copy_failure();
        error!("[xr-copy] dst swapchain image is null; submitting stale image");
        return None;
    }
    // SAFETY: enumerate_imagesで得た有効なXR画像である。
    // openxr-sysのD3D型は不透明なc_voidのためwinapi型へcastする（既存経路と同一）。
    let dst = dst as *mut ID3D11Texture2D;
    // 診断は初回3回と、その後600回に1回だけ行う。
    let seq = DIAG_SEQ.fetch_add(1, Ordering::Relaxed);
    let diagnosing = seq < 3 || seq % 600 == 0;
    // Stage A: D3Dだけで赤を出す。ANGLE・GL・共有面・CopyResourceには触らない。
    if stage_a_enabled() {
        stage_a_fill_red(ready, dst, stats, seq, diagnosing);
        return diagnosing.then_some(seq);
    }
    // E1: 素面へ赤を描く。ページ内容は無視する。
    let device = match contexts.device(context_id) {
        Some(device) => device,
        None => {
            stats.note_copy_failure();
            error!("[xr-copy] no device for E1; submitting stale image");
            return None;
        },
    };
    let (e1_detail, finish_ms) = if e1_enabled() {
        if e1_fill_e3() {
            // E3: TをANGLEに取り込んだSへ赤クリアし、この後の共通転送部で T → XR画像 を写す。
            if let Err(reason) = ensure_e3(contexts, context_id, &device, ready) {
                stats.note_copy_failure();
                if should_emit_fail(seq) {
                    error!("[xr-copy] E3 init failed: {reason}; submitting stale image");
                    emit_json(serde_json::json!({
                        "event": "xr-copy-e3-failed",
                        "seq": seq,
                        "reason": reason,
                    }));
                }
                return None;
            }
            let (w, h) = (ready.width, ready.height);
            // dance中の借用をこのブロックで終え、失敗時の破棄と競合させない。
            let dance: Result<serde_json::Value, String> = {
                let Some(e3) = ready.e3.as_mut() else {
                    stats.note_copy_failure();
                    return None;
                };
                e3_bind_clear_red(contexts, context_id, &device, e3, w, h)
            };
            let detail = match dance {
                Ok(detail) => detail,
                Err(reason) => {
                    // Sの所在が不明な場合があるため裏付けごと破棄し、次フレーム作り直す。
                    if let Some(context) = contexts.context(context_id) {
                        teardown_e3(&device, context, ready);
                    }
                    stats.note_copy_failure();
                    if should_emit_fail(seq) {
                        error!("[xr-copy] E3 red clear failed: {reason}; submitting stale image");
                        emit_json(serde_json::json!({
                            "event": "xr-copy-e3-failed",
                            "seq": seq,
                            "reason": reason,
                        }));
                    }
                    return None;
                },
            };
            if diagnosing {
                emit_json(serde_json::json!({
                    "event": "xr-copy-e3",
                    "seq": seq,
                    "size": {"width": w, "height": h},
                    "detail": detail,
                }));
            }
            (None, 0.0)
        } else if e1_fill_t2() {
            // T2: 自前T(87)をD3Dで赤くし、この後の共通転送部で T → XR画像 を写す。
            // 共有ハンドル由来物は転送元に使わない。
            let t2 = ensure_t2_texture(ready);
            if t2.is_null() {
                stats.note_copy_failure();
                if should_emit_fail(seq) {
                    error!("[xr-copy] T2 texture creation failed; submitting stale image");
                    emit_json(serde_json::json!({
                        "event": "xr-copy-t2",
                        "seq": seq,
                        "ok": false,
                        "reason": "create_texture_failed",
                    }));
                }
                return None;
            }
            d3d_fill_red(ready, t2, "xr-copy-t2", stats, seq, diagnosing);
            (None, 0.0)
        } else if e1_fill_d3d() {
            // T1: D3Dだけで共有面を赤くする。GL・surfmanのbind切り替えは行わない。
            d3d_fill_shared_red(ready, stats, seq, diagnosing);
            (None, 0.0)
        } else {
            match e1_bind_clear_red(contexts, context_id, &device, ready) {
                Ok(detail) => (Some(detail), 0.0),
                Err(reason) => {
                    stats.note_copy_failure();
                    // 失敗分岐のログは初回3回と600フレームに1回に絞る（暴走防止）。
                    if should_emit_fail(seq) {
                        error!("[xr-copy] E1 red clear failed: {reason}; submitting stale image");
                        emit_json(serde_json::json!({
                            "event": "xr-copy-e1-failed",
                            "seq": seq,
                            "reason": reason,
                        }));
                    }
                    return None;
                },
            }
        }
    } else {
        // E2-blit（既定動作）: ページ（自前GLテクスチャ）を確定させ、
        // E3面へbindして `glBlitFramebuffer` で移してから転送する。赤塗りはしない。
        // 旧E2（取り込み面へFBOで描く）は `readPixels [0,0,0,0]` の死角のため使わない。
        match e2_blit_page_to_e3(contexts, context_id, &device, ready) {
            Ok(detail) => {
                let ms = detail
                    .get("finish_ms")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                (Some(detail), ms)
            },
            Err(reason) => {
                // Sの所在が不明な場合があるため裏付けごと破棄し、次フレーム作り直す。
                if let Some(context) = contexts.context(context_id) {
                    teardown_e3(&device, context, ready);
                }
                stats.note_copy_failure();
                if should_emit_fail(seq) {
                    error!("[xr-copy] E2-blit failed: {reason}; submitting stale image");
                    emit_json(serde_json::json!({
                        "event": "xr-copy-e2-blit-failed",
                        "seq": seq,
                        "reason": reason,
                    }));
                }
                return None;
            },
        }
    };
    if diagnosing {
        if e1_enabled() {
            emit_json(serde_json::json!({
                "event": "xr-copy-e1",
                "seq": seq,
                "size": {"width": ready.width, "height": ready.height},
                "detail": e1_detail,
                "resolve": ready.resolve,
            }));
        } else {
            emit_json(serde_json::json!({
                "event": "xr-copy-e2-blit",
                "seq": seq,
                "size": {"width": ready.width, "height": ready.height},
                "detail": e1_detail,
                "resolve": ready.resolve,
            }));
        }
    }
    let begun = Instant::now();
    // E2-blitではE3のTを転送元にする（MISC_SHARED、mutexなし）。
    // T2では共有面ではなく自前Tを転送元にする（CopyResource合法性の切り分け）。
    // SAFETY: immediate context・転送元・転送先はいずれもsetupで確保した有効なD3Dリソースであり、
    // 転送先はacquire済み、転送元はglFinish/blilt/D3Dクリアで確定済みである。同一デバイス上の転送である。
    let src = if !e1_enabled() {
        // E2-blit既定転送。E3のTが無ければstale提出とする（panicさせない）。
        match ready.e3.as_ref() {
            Some(e3) if !e3.t.is_null() => e3.t,
            _ => {
                stats.note_copy_failure();
                if should_emit_fail(seq) {
                    error!("[xr-copy] E2-blit has no E3 texture; submitting stale image");
                }
                return None;
            },
        }
    } else if e1_fill_e3() {
        // E3ではTを転送元にする（T → XR画像の合法性はT2で確定済み）。
        match ready.e3.as_ref() {
            Some(e3) if !e3.t.is_null() => e3.t,
            _ => ready.shared_texture,
        }
    } else if e1_fill_t2() && !ready.t2_texture.is_null() {
        ready.t2_texture
    } else {
        ready.shared_texture
    };
    // 転送元が共有テクスチャTのときはkeyed mutexで括る。E2/T1/E1-danceが該当し、
    // T2（自前T）・E3（MISC_SHAREDのT）は対象外である。キーはsurfmanに合わせた0である。
    // 所有権の受け渡しは acquire(転送前)→release(転送後) の一組であり、
    // releaseは次フレームのANGLE描画への返却を兼ねる（描画前後の括りに相当）。
    let keyed = src == ready.shared_texture && !ready.keyed_mutex.is_null();
    if keyed {
        if let Err(hr) = acquire_shared_texture(ready) {
            stats.note_copy_failure();
            // 失敗分岐のログは初回3回と600フレームに1回に絞る（暴走防止）。
            if should_emit_fail(seq) {
                error!("[xr-copy] keyed mutex acquire failed: {hr:#x}; submitting stale image");
                emit_json(serde_json::json!({
                    "event": "xr-copy-keyedmutex",
                    "seq": seq,
                    "phase": "acquire",
                    "hr": format!("{hr:#x}"),
                }));
            }
            return None;
        }
    }
    unsafe {
        if ready.resolve {
            (*ready.imm_context).ResolveSubresource(
                dst as *mut ID3D11Resource,
                0,
                src as *mut ID3D11Resource,
                0,
                ready.dst_format,
            );
        } else {
            (*ready.imm_context).CopyResource(
                dst as *mut ID3D11Resource,
                src as *mut ID3D11Resource,
            );
        }
        (*ready.imm_context).Flush();
    }
    if keyed {
        if let Err(hr) = release_shared_texture(ready) {
            stats.note_copy_failure();
            // 転送自体は済んでいるため継続するが、次フレームのANGLE描画と競合し得る。
            // 失敗分岐のログは初回3回と600フレームに1回に絞る（暴走防止）。
            if should_emit_fail(seq) {
                error!("[xr-copy] keyed mutex release failed: {hr:#x}");
                emit_json(serde_json::json!({
                    "event": "xr-copy-keyedmutex",
                    "seq": seq,
                    "phase": "release",
                    "hr": format!("{hr:#x}"),
                }));
            }
        }
    }
    let copy_ms = begun.elapsed().as_secs_f64() * 1000.0;
    // SAFETY: setupで保持した有効なデバイスに対する状態取得である。
    let removed = unsafe { (*ready.d3d_device).GetDeviceRemovedReason() };
    if removed != S_OK {
        stats.note_copy_failure();
        // 失敗分岐のログは初回3回と600フレームに1回に絞る（暴走防止）。
        if should_emit_fail(seq) {
            error!("[xr-copy] transfer failed (device reason {removed:#x}); submitting stale image");
        }
    }
    // E1系では素面描画内で済ませているためfinishは0であり、E2-blitではページ確定＋blit詳細内の実測値である。
    stats.record_copy(finish_ms, copy_ms);
    diagnosing.then_some(seq)
}

/// 層の裏付けを解放する。獲得済みD3D参照はここで放す。
/// ページ用GLテクスチャはbindings経由で破棄するため、contextsから都度取得する。
/// 呼び出し側の多重借用を避けるため、device/contextは内部で順次取得する。
pub(super) fn destroy_backing(
    contexts: &mut dyn webxr_api::GLContexts<SurfmanGL>,
    context_id: ContextId,
    backing: CopyBacking,
) {
    let CopyBacking::Ready(mut ready) = backing else {
        return;
    };
    // ページ描画先（自前GLテクスチャ）を破棄する。失敗してもpanicさせず、終了時に回収される。
    if let Some(page) = ready.page_texture.take() {
        if let Some(gl) = contexts.bindings(context_id) {
            // SAFETY: GLスレッド上の有効なコンテキストに対する破棄要求である。
            unsafe {
                gl.delete_texture(page);
            }
        }
    }
    // 以降のsurfman破棄に要るdevice/contextを順次取得する。同時に借りない。
    let Some(device) = contexts.device(context_id) else {
        // SAFETY: D3D参照の放出だけ行う。surfman資源は終了時に回収される。
        unsafe {
            if !ready.t2_texture.is_null() {
                drop(ComPtr::from_raw(ready.t2_texture));
            }
            if !ready.keyed_mutex.is_null() {
                drop(ComPtr::from_raw(ready.keyed_mutex));
            }
            drop(ComPtr::from_raw(ready.shared_texture));
            drop(ComPtr::from_raw(ready.imm_context));
            drop(ComPtr::from_raw(ready.d3d_device));
        }
        if let Some(e3) = ready.e3.take() {
            unsafe {
                if !e3.t.is_null() {
                    drop(ComPtr::from_raw(e3.t));
                }
            }
            if let Some(mut surface) = e3.surface {
                // deviceが無いため破棄できず、終了時に回収される。
                std::mem::forget(surface);
            }
        }
        if let Some(surface_texture) = ready.surface_texture {
            std::mem::forget(surface_texture);
        }
        return;
    };
    let Some(context) = contexts.context(context_id) else {
        // SAFETY: D3D参照の放出だけ行う。
        unsafe {
            if !ready.t2_texture.is_null() {
                drop(ComPtr::from_raw(ready.t2_texture));
            }
            if !ready.keyed_mutex.is_null() {
                drop(ComPtr::from_raw(ready.keyed_mutex));
            }
            drop(ComPtr::from_raw(ready.shared_texture));
            drop(ComPtr::from_raw(ready.imm_context));
            drop(ComPtr::from_raw(ready.d3d_device));
        }
        if let Some(e3) = ready.e3.take() {
            unsafe {
                if !e3.t.is_null() {
                    drop(ComPtr::from_raw(e3.t));
                }
            }
            if let Some(surface) = e3.surface {
                std::mem::forget(surface);
            }
        }
        if let Some(surface_texture) = ready.surface_texture {
            std::mem::forget(surface_texture);
        }
        return;
    };
    // E3裏付け（T＋取り込み面S）を先に破棄する。surface_textureのtake（部分移動）より
    // 後に &mut ready を借りると競合するためである（両者は独立である）。
    teardown_e3(&device, context, &mut ready);
    if let Some(surface_texture) = ready.surface_texture {
        destroy_surface_texture_quiet(&device, context, surface_texture);
    }
    // SAFETY: setup/T2で取得した+1参照をここで放す。GLスレッドでのみ実行される。
    unsafe {
        if !ready.t2_texture.is_null() {
            drop(ComPtr::from_raw(ready.t2_texture));
            ready.t2_texture = ptr::null_mut();
        }
        if !ready.keyed_mutex.is_null() {
            drop(ComPtr::from_raw(ready.keyed_mutex));
            ready.keyed_mutex = ptr::null_mut();
        }
        drop(ComPtr::from_raw(ready.shared_texture));
        drop(ComPtr::from_raw(ready.imm_context));
        drop(ComPtr::from_raw(ready.d3d_device));
    }
}

fn destroy_surface_texture_quiet(
    device: &SurfmanDevice,
    context: &mut SurfmanContext,
    surface_texture: SurfaceTexture,
) {
    match device.destroy_surface_texture(context, surface_texture) {
        Ok(mut surface) => {
            let _ = device.destroy_surface(context, &mut surface);
        },
        Err((_, surface_texture)) => {
            // ADR: 破棄失敗のままDropするとpanicするためリークを選ぶ（probeと同一方針）。
            // 終了時に回収される一時的なGLリソースである。
            std::mem::forget(surface_texture);
        },
    }
}

/// GPUコピー経路の統計。`end_frame`ごとに更新し、一定間隔でJSON出力する。
pub(super) struct CopyStats {
    frames: u64,
    copies: u64,
    copy_failures: u64,
    pub(super) wait_failures: u64,
    pub(super) end_failures: u64,
    sum_finish_ms: f64,
    sum_copy_ms: f64,
    max_finish_ms: f64,
    max_copy_ms: f64,
}

impl CopyStats {
    pub(super) fn new() -> Self {
        Self {
            frames: 0,
            copies: 0,
            copy_failures: 0,
            wait_failures: 0,
            end_failures: 0,
            sum_finish_ms: 0.0,
            sum_copy_ms: 0.0,
            max_finish_ms: 0.0,
            max_copy_ms: 0.0,
        }
    }

    pub(super) fn record_copy(&mut self, finish_ms: f64, copy_ms: f64) {
        self.copies += 1;
        self.sum_finish_ms += finish_ms;
        self.sum_copy_ms += copy_ms;
        self.max_finish_ms = self.max_finish_ms.max(finish_ms);
        self.max_copy_ms = self.max_copy_ms.max(copy_ms);
    }

    pub(super) fn note_copy_failure(&mut self) {
        self.copy_failures += 1;
    }

    pub(super) fn note_wait_failure(&mut self) {
        self.wait_failures += 1;
    }

    pub(super) fn tick_frame(&mut self) {
        self.frames += 1;
        if self.frames.is_multiple_of(EMIT_EVERY_FRAMES) {
            self.emit();
        }
    }

    fn emit(&self) {
        let avg = |sum: f64| {
            if self.copies == 0 {
                0.0
            } else {
                sum / self.copies as f64
            }
        };
        emit_json(serde_json::json!({
            "event": "xr-copy-tick",
            "frames": self.frames,
            "copies": self.copies,
            "copy_failures": self.copy_failures,
            "wait_failures": self.wait_failures,
            "end_failures": self.end_failures,
            "avg_finish_ms": avg(self.sum_finish_ms),
            "avg_copy_ms": avg(self.sum_copy_ms),
            "max_finish_ms": self.max_finish_ms,
            "max_copy_ms": self.max_copy_ms,
        }));
    }
}
