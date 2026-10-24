/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL-2.0/. */

//! OpenXR提示層のファサード。
//!
//! XRift固有のGPUコピー経路（`xr_copy`）の呼び出しを集約し、
//! `mod.rs` のライフサイクル5点からは生成・描画先取得・確定・release・
//! swapchain参照・破棄の呼び出しだけが見えるようにする。
//! 提示方式は [`PresentMode`] で閉じ、層ごとに [`LayerPresent`] で持つ。
//! 提示資源とswapchainは [`PresentLayer`] が一体で所有し、
//! 借用の組み立てをこのモジュール内に閉じる。
//! 統計は [`Presenter`] が層管理単位で集約する。

use std::collections::HashMap;
use std::sync::Arc;

use euclid::{Rect, Size2D};
use glow::{self as gl, HasContext};
use log::error;
use openxr::{Graphics, Session, Swapchain};
use surfman::{
    Context as SurfmanContext, Device as SurfmanDevice, Error as SurfmanError, SurfaceTexture,
};
use webxr_api::{ContextId, Error, GLContexts, LayerId, Viewport, Viewports};

use crate::SurfmanGL;

use super::graphics::{GraphicsProvider, GraphicsProviderMethods};
use super::Backend;
use super::xr_copy;

/// 移動前のログtarget。`mod.rs` にあった呼び出しの観測互換（`RUST_LOG` 絞り込み等）を保つ。
/// W3で診断を整理するまで変えない。
const OLD_LOG_TARGET: &str = "servo_webxr::openxr";

#[cfg(target_os = "windows")]
pub(super) mod probe;

/// `create_layer` 先頭の計測プローブ。Windowsでのみ実行する。
pub(super) fn probe_layer_once(
    session: &Arc<Session<Backend>>,
    contexts: &mut dyn GLContexts<SurfmanGL>,
    context_id: ContextId,
) {
    // 計測自体は `XR_COPY_DIAG=1` のときだけ行う。既定では呼ばない（W3）。
    // `XR_PROBE_LOG` は従来どおり、有効時の追記先として残す。
    #[cfg(target_os = "windows")]
    if xr_copy::diag_enabled() {
        probe::probe_once(session, contexts, context_id);
    }
    #[cfg(not(target_os = "windows"))]
    let _ = (session, contexts, context_id);
}

/// 描画先の所有値。GLテクスチャとその標的だけを持ち、参照は保持しない。
pub(super) struct DrawTarget {
    pub(super) color_texture: Option<gl::NativeTexture>,
    pub(super) color_target: u32,
}

/// 層管理単位の提示方式。[`Presenter`] の生成時に確定する。
#[derive(Clone, Copy)]
pub(super) enum PresentMode {
    /// 従来のimport経路。XR画像を待って直接描く。
    Import,
    /// GPUコピー経路。共有pbufferへ描き、`end_frame` でXR画像へ転送する。
    #[cfg(target_os = "windows")]
    GpuCopy,
}

impl PresentMode {
    /// 提示方式を決める。`XR_LAYER_MODE` はここで1回だけ読む。
    ///
    /// 実行中に環境変数を変えても反映しない。これは意図した設定契約の変更であり、
    /// 従来の層生成・end_frameのたびの再読込は行わない。自動フォールバックは追加しない。
    fn from_env() -> Self {
        #[cfg(target_os = "windows")]
        if std::env::var("XR_LAYER_MODE")
            .map(|v| v != "import")
            .unwrap_or(true)
        {
            return Self::GpuCopy;
        }
        Self::Import
    }
}

/// 提示全体の管理単位。生成時に確定した方式と、GPUコピー経路の統計を集約する。
pub(super) struct Presenter {
    mode: PresentMode,
    #[cfg(target_os = "windows")]
    stats: xr_copy::CopyStats,
}

impl Presenter {
    pub(super) fn new() -> Self {
        Presenter {
            mode: PresentMode::from_env(),
            #[cfg(target_os = "windows")]
            stats: xr_copy::CopyStats::new(),
        }
    }

    /// 生成時に確定した提示方式を返す。
    pub(super) fn mode(&self) -> PresentMode {
        self.mode
    }

    /// `end_frame`・`end_secondary` の提出失敗を統計に記録する。
    pub(super) fn note_end_failure(&mut self) {
        #[cfg(target_os = "windows")]
        {
            self.stats.end_failures += 1;
        }
    }

    /// 層の描画先を取得する。`begin_frame` の層走査から呼ぶ。
    /// 取得順序（device→context→層→acquire）は移動前と同一に保つ。
    pub(super) fn acquire_draw_target(
        &mut self,
        present_layers: &mut HashMap<LayerId, PresentLayer>,
        contexts: &mut dyn GLContexts<SurfmanGL>,
        context_id: ContextId,
        layer_id: LayerId,
    ) -> Result<DrawTarget, Error> {
        let device = contexts.device(context_id).ok_or_else(|| {
            // 移動前のtargetを保つ（W3で整理）。
            error!(target: OLD_LOG_TARGET, "[xr-debug] no device for context {context_id:?}");
            Error::NoMatchingDevice
        })?;
        let context = contexts.context(context_id).ok_or_else(|| {
            // 移動前のtargetを保つ（W3で整理）。
            error!(target: OLD_LOG_TARGET, "[xr-debug] no surfman context for {context_id:?}");
            Error::NoMatchingDevice
        })?;
        let layer = present_layers.get_mut(&layer_id).ok_or_else(|| {
            // 移動前のtargetを保つ（W3で整理）。
            error!(target: OLD_LOG_TARGET, "[xr-debug] no openxr layer for {layer_id:?}");
            Error::NoMatchingDevice
        })?;

        let image = match layer.swapchain.acquire_image() {
            Ok(image) => image,
            Err(e) => {
                // 移動前のtargetを保つ（W3で整理）。
                error!(target: OLD_LOG_TARGET, "[xr-debug] acquire_image failed: {:?}", e);
                return Err(Error::BackendSpecific(format!(
                    "Swapchain::acquire_image {:?}",
                    e
                )));
            },
        };
        // GPUコピー経路の層は共有pbufferへ描く。XR画像への転送はend_frameで行う。
        // 層の提示方式で分岐する。方式は生成時に確定済みのため、ここで環境変数は読まない。
        let (color_texture, color_target) = match &mut layer.present {
            LayerPresent::Import(import) => {
                if let Err(e) = layer.swapchain.wait_image(openxr::Duration::INFINITE) {
                    // 移動前のtargetを保つ（W3で整理）。
                    error!(target: OLD_LOG_TARGET, "[xr-debug] wait_image failed: {:?}", e);
                    return Err(Error::BackendSpecific(format!(
                        "Swapchain::wait_image {:?}",
                        e
                    )));
                }
                layer.waited = true;

                let color_surface_texture = match import.get_surface_texture(
                    &layer.images,
                    &layer.size,
                    &device,
                    context,
                    image as usize,
                ) {
                    Ok(texture) => texture,
                    Err(e) => {
                        // 移動前のtargetを保つ（W3で整理）。
                        error!(target: OLD_LOG_TARGET, "[xr-debug] get_surface_texture failed: {:?}", e);
                        return Err(Error::BackendSpecific(format!(
                            "Layer::get_surface_texture {:?}",
                            e
                        )));
                    },
                };
                let color_texture = device.surface_texture_object(color_surface_texture);
                let color_target = device.surface_gl_texture_target();
                (color_texture, color_target)
            },
            #[cfg(target_os = "windows")]
            LayerPresent::GpuCopy(gpu) => {
                if matches!(gpu.backing, xr_copy::CopyBacking::Broken) {
                    // 移動前のtargetを保つ（W3で整理）。
                    error!(target: OLD_LOG_TARGET, "[xr-debug] copy backing is broken; skipping frame");
                    return Err(Error::BackendSpecific(
                        "Layer GPU-copy backing is broken".into(),
                    ));
                }
                // backingとswapchainは別フィールドのため同時借用できる。
                let ready = match &mut gpu.backing {
                    xr_copy::CopyBacking::Ready(ready) => ready,
                    // 直上でBrokenを弾いているため到達しない。
                    _ => {
                        // 移動前のtargetを保つ（W3で整理）。
                        error!(target: OLD_LOG_TARGET, "[xr-debug] copy backing vanished; skipping frame");
                        return Err(Error::BackendSpecific(
                            "Layer GPU-copy backing vanished".into(),
                        ));
                    },
                };
                if let Err(e) = xr_copy::wait_for_copy(&mut layer.swapchain, ready, image) {
                    self.stats.note_wait_failure();
                    // 移動前のtargetを保つ（W3で整理）。
                    error!(target: OLD_LOG_TARGET, "[xr-debug] wait_image failed: {:?}", e);
                    return Err(e);
                }
                layer.waited = true;
                // E2-blit既定では自前の通常テクスチャへ描く。旧E2の取り込み面は使わない。
                // ページは今まで通り `XRWebGLLayer.framebuffer` に描き、end_frameでblitする。
                if let Some(page) = ready.page_texture {
                    (Some(page), ready.page_target)
                } else {
                    (Some(ready.gl_texture), ready.gl_target)
                }
            },
        };
        Ok(DrawTarget {
            color_texture,
            color_target,
        })
    }

    /// `end_frame` の描画確定部。全層のコピーを終えてから統計を進める。
    /// 提出診断のための `(seq, 幅, 高さ, 形式, 標本数)` を層の順に返す。
    pub(super) fn finish_render(
        &mut self,
        contexts: &mut dyn GLContexts<SurfmanGL>,
        layers: &[(ContextId, LayerId)],
        present_layers: &mut HashMap<LayerId, PresentLayer>,
    ) -> Vec<(u64, u32, u32, u32, u32)> {
        let mut diag_submits = Vec::new();
        // WindowsのGPUコピー経路: 描画確定後に共有pbufferの内容をXR画像へ転送する。
        // CPU読み戻しは行わない。失敗してもpanicせず、その層の提出はstale画像のまま継続する。
        // E1では素面へ赤を描いて転送するため、contextsごと渡してbind切り替えを行う。
        match self.mode {
            PresentMode::Import => {},
            #[cfg(target_os = "windows")]
            PresentMode::GpuCopy => {
                let stats = &mut self.stats;
                for &(context_id, layer_id) in layers {
                    let Some(layer) = present_layers.get_mut(&layer_id) else {
                        continue;
                    };
                    let LayerPresent::GpuCopy(gpu) = &mut layer.present else {
                        continue;
                    };
                    let xr_copy::CopyBacking::Ready(ready) = &mut gpu.backing else {
                        continue;
                    };
                    let Some(index) = ready.acquired_image.take() else {
                        continue;
                    };
                    let dst = layer
                        .images
                        .get(index as usize)
                        .copied()
                        .unwrap_or(std::ptr::null_mut());
                    if let Some(seq) =
                        xr_copy::copy_to_swapchain(contexts, context_id, ready, dst, stats)
                    {
                        if let Some((w, h, f, s)) = xr_copy::describe_image(dst) {
                            diag_submits.push((seq, w, h, f, s));
                        }
                    }
                }
                stats.tick_frame();
            },
        }
        #[cfg(not(target_os = "windows"))]
        let _ = (contexts, layers, present_layers);
        diag_submits
    }

    /// 提出診断JSONを出す。全コピー完了後・統計進行後に呼ぶ。
    /// 診断の有効条件・頻度・既定出力は `xr_copy::emit_json` に従い変えない。
    pub(super) fn emit_submit_diag(
        submits: &[(u64, u32, u32, u32, u32)],
        viewports: &Viewports,
    ) {
        #[cfg(target_os = "windows")]
        {
            let rect = |v: Rect<i32, Viewport>| {
                serde_json::json!({
                    "x": v.origin.x,
                    "y": v.origin.y,
                    "width": v.size.width,
                    "height": v.size.height,
                })
            };
            for (seq, w, h, f, s) in submits {
                xr_copy::emit_json(serde_json::json!({
                    "event": "xr-copy-submit",
                    "seq": seq,
                    "sub_image": {
                        "image_rect": rect(viewports.viewports[0]),
                        "image_array_index": 0u32,
                    },
                    "swapchain": {
                        "width": w,
                        "height": h,
                        "format": f,
                        "samples": s,
                    },
                    "viewports": viewports.viewports.iter().map(|v| rect(*v)).collect::<Vec<_>>(),
                }));
            }
        }
        #[cfg(not(target_os = "windows"))]
        let _ = (submits, viewports);
    }
}

/// 層ごとの提示方式。生成時に確定し、以後は変わらない。
enum LayerPresent {
    /// 従来のimport経路。面テクスチャのキャッシュを持つ。
    Import(ImportPresent),
    /// GPUコピー経路。裏付けの成否（Ready/Broken）を保持する。
    #[cfg(target_os = "windows")]
    GpuCopy(GpuCopyPresent),
}

/// 従来のimport経路の層資源。XR画像から作った面テクスチャを層ごとに保持する。
struct ImportPresent {
    surface_textures: Vec<Option<SurfaceTexture>>,
}

impl ImportPresent {
    fn new(image_count: usize) -> Self {
        let mut surface_textures = Vec::new();
        surface_textures.resize_with(image_count, || None);
        Self { surface_textures }
    }

    fn get_surface_texture(
        &mut self,
        images: &[<Backend as Graphics>::SwapchainImage],
        size: &Size2D<i32, Viewport>,
        device: &SurfmanDevice,
        context: &mut SurfmanContext,
        index: usize,
    ) -> Result<&SurfaceTexture, SurfmanError> {
        let result = self
            .surface_textures
            .get_mut(index)
            .ok_or(SurfmanError::Failed)?;
        if let Some(result) = result {
            return Ok(result);
        }
        let surface_texture = GraphicsProvider::surface_texture_from_swapchain_texture(
            images[index],
            device,
            context,
            &size.to_untyped(),
        )?;
        *result = Some(surface_texture);
        result.as_ref().ok_or(SurfmanError::Failed)
    }
}

/// GPUコピー経路の層資源。裏付けは非Optionで持ち、生成失敗は `Broken` で表す。
#[cfg(target_os = "windows")]
struct GpuCopyPresent {
    backing: xr_copy::CopyBacking,
}

#[cfg(target_os = "windows")]
impl GpuCopyPresent {
    fn new(
        contexts: &mut dyn GLContexts<SurfmanGL>,
        context_id: ContextId,
        images: &[<Backend as Graphics>::SwapchainImage],
        width: i32,
        height: i32,
        format: u32,
        sample_count: u32,
    ) -> Self {
        Self {
            backing: xr_copy::setup_backing(
                contexts,
                context_id,
                images,
                width,
                height,
                format,
                sample_count,
            ),
        }
    }
}

/// 層ごとの提示資源。swapchainと列挙画像・wait状態・方式別の提示資源を一体で所有する。
pub(super) struct PresentLayer {
    swapchain: Swapchain<Backend>,
    depth_stencil_texture: Option<gl::NativeTexture>,
    size: Size2D<i32, Viewport>,
    images: Vec<<Backend as Graphics>::SwapchainImage>,
    waited: bool,
    present: LayerPresent,
}

impl PresentLayer {
    /// 層資源を生成する。提示方式は `mode` で確定し、裏付けまで一体で作る。
    pub(super) fn new(
        mode: PresentMode,
        contexts: &mut dyn GLContexts<SurfmanGL>,
        context_id: ContextId,
        swapchain: Swapchain<Backend>,
        depth_stencil_texture: Option<gl::NativeTexture>,
        size: Size2D<i32, Viewport>,
        format: u32,
        sample_count: u32,
    ) -> Result<PresentLayer, Error> {
        let images = swapchain
            .enumerate_images()
            .map_err(|e| Error::BackendSpecific(format!("Session::enumerate_images {:?}", e)))?;
        let waited = false;
        let present = match mode {
            PresentMode::Import => LayerPresent::Import(ImportPresent::new(images.len())),
            #[cfg(target_os = "windows")]
            PresentMode::GpuCopy => LayerPresent::GpuCopy(GpuCopyPresent::new(
                contexts,
                context_id,
                &images,
                size.width,
                size.height,
                format,
                sample_count,
            )),
        };
        Ok(PresentLayer {
            swapchain,
            depth_stencil_texture,
            size,
            images,
            waited,
            present,
        })
    }

    /// swapchainへの共有参照を返す。提出層の組み立てから使う。
    pub(super) fn swapchain(&self) -> &Swapchain<Backend> {
        &self.swapchain
    }

    /// 深度・ステンシル面を返す。`begin_frame` の描画情報組み立てから使う。
    pub(super) fn depth_stencil_texture(&self) -> Option<gl::NativeTexture> {
        self.depth_stencil_texture
    }

    /// 層寸法を返す。`begin_frame` の描画情報組み立てから使う。
    pub(super) fn size(&self) -> Size2D<i32, Viewport> {
        self.size
    }

    /// 層の裏付けを解放する。device/contextの取得より先にGPUコピー裏付けを放す。
    pub(super) fn destroy(
        self,
        contexts: &mut dyn GLContexts<SurfmanGL>,
        context_id: ContextId,
    ) {
        if let Some(depth_stencil_texture) = self.depth_stencil_texture {
            let gl = contexts.bindings(context_id).unwrap();
            unsafe { gl.delete_texture(depth_stencil_texture) };
        }
        // GPUコピー経路の裏付けを先に解放する。device/contextの借用と競合させないため先に行う。
        let import = match self.present {
            LayerPresent::Import(import) => Some(import),
            #[cfg(target_os = "windows")]
            LayerPresent::GpuCopy(gpu) => {
                xr_copy::destroy_backing(contexts, context_id, gpu.backing);
                None
            },
        };
        let device = contexts
            .device(context_id)
            .expect("missing device for context");
        let context = contexts.context(context_id).expect("missing GL context");
        // 所有権ごと受け取っているため、空vecとの置換は要らない。
        if let Some(import) = import {
            for surface_texture in import.surface_textures {
                if let Some(surface_texture) = surface_texture {
                    let mut surface = device
                        .destroy_surface_texture(context, surface_texture)
                        .unwrap();
                    device.destroy_surface(context, &mut surface).unwrap();
                }
            }
        }
    }

    /// wait済み画像を提出前に離す。`end_frame` のrelease走査から呼ぶ。
    pub(super) fn release_waited_image(&mut self) -> Result<(), Error> {
        if self.waited {
            self.swapchain.release_image().map_err(|e| {
                Error::BackendSpecific(format!("Session::release_image {:?}", e))
            })?;
            self.waited = false;
        }
        Ok(())
    }
}
