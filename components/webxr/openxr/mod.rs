/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{mem, thread};

use euclid::{Point2D, Rect, RigidTransform3D, Rotation3D, Size2D, Transform3D, Vector3D};
use glow::{self as gl, HasContext, PixelUnpackData};
use interaction_profiles::{get_profiles_from_path, get_supported_interaction_profiles};
use log::{error, info, warn};
use openxr::sys::CompositionLayerPassthroughFB;
use openxr::{
    self, ActionSet, ActiveActionSet, ApplicationInfo, CompositionLayerBase, CompositionLayerFlags,
    CompositionLayerProjection, Entry, EnvironmentBlendMode, ExtensionSet, Extent2Di, FormFactor,
    Fovf, FrameState, FrameStream, FrameWaiter, Graphics, Instance, Passthrough,
    PassthroughFlagsFB, PassthroughLayerFB, PassthroughLayerPurposeFB, Posef, Quaternionf,
    ReferenceSpaceType, SecondaryEndInfo, Session, Space, Swapchain, SwapchainCreateFlags,
    SwapchainCreateInfo, SwapchainUsageFlags, SystemId, Vector3f, Version, ViewConfigurationType,
};
use profile_traits::generic_callback::GenericCallback as ProfileGenericCallback;
use surfman::{
    Context as SurfmanContext, Device as SurfmanDevice, Error as SurfmanError, SurfaceTexture,
};
use webxr_api::util::{self, ClipPlanes};
use webxr_api::{
    self, BaseSpace, Capture, ContextId, DeviceAPI, DiscoveryAPI, Display, Error, Event,
    EventBuffer, Floor, Frame, GLContexts, InputId, InputSource, LayerGrandManager, LayerId,
    LayerInit, LayerManager, LayerManagerAPI, LeftEye, Native, Quitter, RightEye, SelectKind,
    Session as WebXrSession, SessionBuilder, SessionInit, SessionMode, SubImage, SubImages, View,
    ViewerPose, Viewport, Viewports, Views, Visibility,
};

use crate::SurfmanGL;
use crate::gl_utils::GlClearer;

mod input;
use input::OpenXRInput;
mod graphics;
mod interaction_profiles;
use graphics::{GraphicsProvider, GraphicsProviderMethods};
#[cfg(target_os = "windows")]
mod xr_copy;

#[cfg(target_os = "windows")]
mod graphics_d3d11;
#[cfg(target_os = "windows")]
use graphics_d3d11::Backend;

const HEIGHT: f32 = 1.4;

const IDENTITY_POSE: Posef = Posef {
    orientation: Quaternionf {
        x: 0.,
        y: 0.,
        z: 0.,
        w: 1.,
    },
    position: Vector3f {
        x: 0.,
        y: 0.,
        z: 0.,
    },
};

const VIEW_INIT: openxr::View = openxr::View {
    pose: IDENTITY_POSE,
    fov: Fovf {
        angle_left: 0.,
        angle_right: 0.,
        angle_up: 0.,
        angle_down: 0.,
    },
};

// How much to downscale the view capture by.
// This is used for performance reasons, to dedicate less texture memory to the camera.
// Note that on an HL2 this allocates enough texture memory for "low power" mode,
// not "high quality" (in the device portal under
// Views > Mixed Reality Capture > Photo and Video Settings).
const SECONDARY_VIEW_DOWNSCALE: i32 = 2;

/// Provides a way to spawn and interact with context menus
pub trait ContextMenuProvider: Send {
    /// Open a context menu, return a way to poll for the result
    fn open_context_menu(&self) -> Box<dyn ContextMenuFuture>;
    /// Clone self as a trait object
    fn clone_object(&self) -> Box<dyn ContextMenuProvider>;
}

/// A way to poll for the result of the context menu request
pub trait ContextMenuFuture {
    fn poll(&self) -> ContextMenuResult;
}

/// The result of polling on a context menu request
pub enum ContextMenuResult {
    /// Session should exit
    ExitSession,
    /// Dialog was dismissed
    Dismissed,
    /// User has not acted on dialog
    Pending,
}

#[derive(Default)]
pub struct AppInfo {
    application_name: String,
    application_version: u32,
    engine_name: String,
    engine_version: u32,
}

impl AppInfo {
    pub fn new(
        application_name: &str,
        application_version: u32,
        engine_name: &str,
        engine_version: u32,
    ) -> AppInfo {
        Self {
            application_name: application_name.to_string(),
            application_version,
            engine_name: engine_name.to_string(),
            engine_version,
        }
    }
}

struct ViewInfo<Eye> {
    view: openxr::View,
    extent: Extent2Di,
    cached_projection: Transform3D<f32, Eye, Display>,
}

impl<Eye> ViewInfo<Eye> {
    fn set_view(&mut self, view: openxr::View, clip_planes: ClipPlanes) {
        self.view.pose = view.pose;
        if self.view.fov.angle_left != view.fov.angle_left ||
            self.view.fov.angle_right != view.fov.angle_right ||
            self.view.fov.angle_up != view.fov.angle_up ||
            self.view.fov.angle_down != view.fov.angle_down
        {
            // It's fine if this happens occasionally, but if this happening very
            // often we should stop caching
            warn!("FOV changed, updating projection matrices");
            self.view.fov = view.fov;
            self.recompute_projection(clip_planes);
        }
    }

    fn recompute_projection(&mut self, clip_planes: ClipPlanes) {
        self.cached_projection = fov_to_projection_matrix(&self.view.fov, clip_planes);
    }

    fn view(&self) -> View<Eye> {
        View {
            transform: transform(&self.view.pose),
            projection: self.cached_projection,
        }
    }
}

pub struct OpenXrDiscovery {
    context_menu_provider: Option<Box<dyn ContextMenuProvider>>,
    app_info: AppInfo,
}

impl OpenXrDiscovery {
    pub fn new(
        context_menu_provider: Option<Box<dyn ContextMenuProvider>>,
        app_info: AppInfo,
    ) -> Self {
        Self {
            context_menu_provider,
            app_info,
        }
    }
}

pub struct CreatedInstance {
    instance: Instance,
    supports_hands: bool,
    supports_secondary: bool,
    system: SystemId,
    supports_mutable_fov: bool,
    supported_interaction_profiles: Vec<&'static str>,
    supports_passthrough: bool,
    supports_updating_framerate: bool,
}

fn create_instance(
    needs_hands: bool,
    needs_secondary: bool,
    needs_passthrough: bool,
    app_info: &AppInfo,
) -> Result<CreatedInstance, String> {
    let entry = unsafe { Entry::load().map_err(|e| format!("Entry::load {:?}", e))? };
    let supported = entry
        .enumerate_extensions()
        .map_err(|e| format!("Entry::enumerate_extensions {:?}", e))?;
    warn!("Available extensions:\n{:?}", supported);
    let mut supports_hands = needs_hands && supported.ext_hand_tracking;
    let supports_passthrough = needs_passthrough && supported.fb_passthrough;
    let supports_secondary = needs_secondary &&
        supported.msft_secondary_view_configuration &&
        supported.msft_first_person_observer;
    let supports_updating_framerate = supported.fb_display_refresh_rate;

    let app_info = ApplicationInfo {
        application_name: &app_info.application_name,
        application_version: app_info.application_version,
        engine_name: &app_info.engine_name,
        engine_version: app_info.engine_version,
        api_version: Version::new(1, 0, 36),
    };

    let mut exts = ExtensionSet::default();
    GraphicsProvider::enable_graphics_extensions(&mut exts);
    if supports_hands {
        exts.ext_hand_tracking = true;
    }

    if supports_secondary {
        exts.msft_secondary_view_configuration = true;
        exts.msft_first_person_observer = true;
    }

    if supports_passthrough {
        exts.fb_passthrough = true;
    }

    if supports_updating_framerate {
        exts.fb_display_refresh_rate = true;
    }

    let supported_interaction_profiles = get_supported_interaction_profiles(&supported, &mut exts);

    let instance = entry
        .create_instance(&app_info, &exts, &[])
        .map_err(|e| format!("Entry::create_instance {:?}", e))?;
    let system = instance
        .system(FormFactor::HEAD_MOUNTED_DISPLAY)
        .map_err(|e| format!("Instance::system {:?}", e))?;

    if supports_hands {
        supports_hands |= instance
            .supports_hand_tracking(system)
            .map_err(|e| format!("Instance::supports_hand_tracking {:?}", e))?;
    }

    let supports_mutable_fov = {
        let properties = instance
            .view_configuration_properties(system, ViewConfigurationType::PRIMARY_STEREO)
            .map_err(|e| format!("Instance::view_configuration_properties {:?}", e))?;
        // Unfortunately we need to do a platform check here as just flipping the FOVs for the
        // composition layer is seemingly no longer sufficient. As long as windows sessions are
        // solely backed by D3D11, we'll need to apply the same inverted view + reversed winding
        // fix for SteamVR as well as Oculus.
        properties.fov_mutable && !cfg!(target_os = "windows")
    };

    Ok(CreatedInstance {
        instance,
        supports_hands,
        supports_secondary,
        system,
        supports_mutable_fov,
        supported_interaction_profiles,
        supports_passthrough,
        supports_updating_framerate,
    })
}

impl DiscoveryAPI<SurfmanGL> for OpenXrDiscovery {
    fn request_session(
        &mut self,
        mode: SessionMode,
        init: &SessionInit,
        xr: SessionBuilder<SurfmanGL>,
    ) -> Result<WebXrSession, Error> {
        if self.supports_session(mode) {
            let needs_hands = init.feature_requested("hand-tracking");
            let needs_secondary =
                init.feature_requested("secondary-views") && init.first_person_observer_view;
            let needs_passthrough = mode == SessionMode::ImmersiveAR;
            let instance = create_instance(
                needs_hands,
                needs_secondary,
                needs_passthrough,
                &self.app_info,
            )
            .map_err(|e| Error::BackendSpecific(e))?;

            let mut supported_features = vec!["local-floor".into(), "bounded-floor".into()];
            if instance.supports_hands {
                supported_features.push("hand-tracking".into());
            }
            if instance.supports_secondary && init.first_person_observer_view {
                supported_features.push("secondary-views".into());
            }
            let granted_features = init.validate(mode, &supported_features)?;
            let context_menu_provider = self.context_menu_provider.take();
            xr.spawn(move |grand_manager| {
                OpenXrDevice::new(
                    instance,
                    granted_features,
                    context_menu_provider,
                    grand_manager,
                )
            })
        } else {
            Err(Error::NoMatchingDevice)
        }
    }

    fn supports_session(&self, mode: SessionMode) -> bool {
        let mut supports = false;
        // Determining AR support requires enumerating environment blend modes,
        // but this requires an already created XrInstance and SystemId.
        // We'll make a "default" instance here to check the blend modes,
        // then a proper one in request_session with hands/secondary support if needed.
        let needs_passthrough = mode == SessionMode::ImmersiveAR;
        if let Ok(instance) = create_instance(false, false, needs_passthrough, &self.app_info) {
            if let Ok(blend_modes) = instance.instance.enumerate_environment_blend_modes(
                instance.system,
                ViewConfigurationType::PRIMARY_STEREO,
            ) {
                if mode == SessionMode::ImmersiveAR {
                    supports = blend_modes.contains(&EnvironmentBlendMode::ADDITIVE) ||
                        blend_modes.contains(&EnvironmentBlendMode::ALPHA_BLEND) ||
                        instance.supports_passthrough;
                } else if mode == SessionMode::ImmersiveVR {
                    // Immersive VR sessions are not precluded by non-opaque blending
                    supports = blend_modes.len() > 0;
                }
            }
        }
        supports
    }
}

struct OpenXrDevice {
    session: Arc<Session<Backend>>,
    instance: Instance,
    events: EventBuffer,
    frame_waiter: FrameWaiter,
    layer_manager: LayerManager,
    viewer_space: Space,
    shared_data: Arc<Mutex<Option<SharedData>>>,
    clip_planes: ClipPlanes,
    supports_secondary: bool,
    supports_mutable_fov: bool,
    supports_updating_framerate: bool,

    // input
    action_set: ActionSet,
    right_hand: OpenXRInput,
    left_hand: OpenXRInput,
    granted_features: Vec<String>,
    context_menu_provider: Option<Box<dyn ContextMenuProvider>>,
    context_menu_future: Option<Box<dyn ContextMenuFuture>>,
}

/// Data that is shared between the openxr thread and the
/// layer manager that runs in the webgl thread.
struct SharedData {
    left: ViewInfo<LeftEye>,
    right: ViewInfo<RightEye>,
    secondary: Option<ViewInfo<Capture>>,
    secondary_active: bool,
    primary_blend_mode: EnvironmentBlendMode,
    secondary_blend_mode: Option<EnvironmentBlendMode>,
    frame_state: Option<FrameState>,
    space: Space,
    swapchain_sample_count: u32,
}

struct OpenXrLayerManager {
    session: Arc<Session<Backend>>,
    shared_data: Arc<Mutex<Option<SharedData>>>,
    frame_stream: FrameStream<Backend>,
    layers: Vec<(ContextId, LayerId)>,
    openxr_layers: HashMap<LayerId, OpenXrLayer>,
    clearer: GlClearer,
    _passthrough: Option<Passthrough>,
    passthrough_layer: Option<PassthroughLayerFB>,
    #[cfg(target_os = "windows")]
    copy_stats: xr_copy::CopyStats,
}

struct OpenXrLayer {
    swapchain: Swapchain<Backend>,
    depth_stencil_texture: Option<gl::NativeTexture>,
    size: Size2D<i32, Viewport>,
    images: Vec<<Backend as Graphics>::SwapchainImage>,
    surface_textures: Vec<Option<SurfaceTexture>>,
    waited: bool,
    #[cfg(target_os = "windows")]
    copy_backing: Option<xr_copy::CopyBacking>,
}

impl OpenXrLayerManager {
    fn new(
        session: Arc<Session<Backend>>,
        shared_data: Arc<Mutex<Option<SharedData>>>,
        frame_stream: FrameStream<Backend>,
        should_reverse_winding: bool,
        _passthrough: Option<Passthrough>,
        passthrough_layer: Option<PassthroughLayerFB>,
    ) -> OpenXrLayerManager {
        let layers = Vec::new();
        let openxr_layers = HashMap::new();
        let clearer = GlClearer::new(should_reverse_winding);
        OpenXrLayerManager {
            session,
            shared_data,
            frame_stream,
            layers,
            openxr_layers,
            clearer,
            _passthrough,
            passthrough_layer,
            #[cfg(target_os = "windows")]
            copy_stats: xr_copy::CopyStats::new(),
        }
    }
}

impl OpenXrLayer {
    fn new(
        swapchain: Swapchain<Backend>,
        depth_stencil_texture: Option<gl::NativeTexture>,
        size: Size2D<i32, Viewport>,
    ) -> Result<OpenXrLayer, Error> {
        let images = swapchain
            .enumerate_images()
            .map_err(|e| Error::BackendSpecific(format!("Session::enumerate_images {:?}", e)))?;
        let waited = false;
        let mut surface_textures = Vec::new();
        surface_textures.resize_with(images.len(), || None);
        Ok(OpenXrLayer {
            swapchain,
            depth_stencil_texture,
            size,
            images,
            surface_textures,
            waited,
            #[cfg(target_os = "windows")]
            copy_backing: None,
        })
    }

    fn get_surface_texture(
        &mut self,
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
            self.images[index],
            device,
            context,
            &self.size.to_untyped(),
        )?;
        *result = Some(surface_texture);
        result.as_ref().ok_or(SurfmanError::Failed)
    }
}

impl LayerManagerAPI<SurfmanGL> for OpenXrLayerManager {
    fn create_layer(
        &mut self,
        contexts: &mut dyn GLContexts<SurfmanGL>,
        context_id: ContextId,
        init: LayerInit,
    ) -> Result<LayerId, Error> {
        // 計測用プローブ(後で削除予定)。最初に呼ばれた1回だけ共有可否を記録する。
        // 画素の読み戻しはせず、失敗しても先へ進む。
        #[cfg(target_os = "windows")]
        xr_probe::probe_once(&self.session, contexts, context_id);
        let guard = self.shared_data.lock().unwrap();
        let data = guard.as_ref().unwrap();

        // XXXManishearth should we be doing this, or letting Servo set the format?
        let formats = self.session.enumerate_swapchain_formats().map_err(|e| {
            Error::BackendSpecific(format!("Session::enumerate_swapchain_formats {:?}", e))
        })?;
        let format = GraphicsProvider::pick_format(&formats);
        let texture_size = init.texture_size(&data.viewports());
        let sample_count = data.swapchain_sample_count;
        let swapchain_create_info = SwapchainCreateInfo {
            create_flags: SwapchainCreateFlags::EMPTY,
            usage_flags: SwapchainUsageFlags::COLOR_ATTACHMENT | SwapchainUsageFlags::SAMPLED,
            width: texture_size.width as u32,
            height: texture_size.height as u32,
            format,
            sample_count,
            face_count: 1,
            array_size: 1,
            mip_count: 1,
        };
        let swapchain = self
            .session
            .create_swapchain(&swapchain_create_info)
            .map_err(|e| Error::BackendSpecific(format!("Session::create_swapchain {:?}", e)))?;

        // TODO: Treat depth and stencil separately?
        // TODO: Use the openxr API for depth/stencil swap chains?
        let has_depth_stencil = match init {
            LayerInit::WebGLLayer { stencil, depth, .. } => stencil | depth,
            LayerInit::ProjectionLayer { stencil, depth, .. } => stencil | depth,
        };
        let depth_stencil_texture = if has_depth_stencil {
            let gl = contexts
                .bindings(context_id)
                .ok_or(Error::NoMatchingDevice)?;
            unsafe {
                let depth_stencil_texture = gl.create_texture().ok();
                gl.bind_texture(gl::TEXTURE_2D, depth_stencil_texture);
                gl.tex_image_2d(
                    gl::TEXTURE_2D,
                    0,
                    gl::DEPTH24_STENCIL8 as _,
                    texture_size.width,
                    texture_size.height,
                    0,
                    gl::DEPTH_STENCIL,
                    gl::UNSIGNED_INT_24_8,
                    PixelUnpackData::Slice(None),
                );
                depth_stencil_texture
            }
        } else {
            None
        };

        let layer_id = LayerId::default();
        let openxr_layer = OpenXrLayer::new(swapchain, depth_stencil_texture, texture_size)?;
        #[cfg(target_os = "windows")]
        let mut openxr_layer = openxr_layer;
        // Windowsでは既定でGPUコピー経路を使い、共有pbufferを描画先とする。
        // `XR_LAYER_MODE=import`のときだけ従来のimport経路に戻す。
        #[cfg(target_os = "windows")]
        if xr_copy::copy_mode_enabled() {
            openxr_layer.copy_backing = Some(xr_copy::setup_backing(
                contexts,
                context_id,
                &openxr_layer.images,
                texture_size.width,
                texture_size.height,
                format,
                sample_count,
            ));
        }
        self.layers.push((context_id, layer_id));
        self.openxr_layers.insert(layer_id, openxr_layer);
        Ok(layer_id)
    }

    fn destroy_layer(
        &mut self,
        contexts: &mut dyn GLContexts<SurfmanGL>,
        context_id: ContextId,
        layer_id: LayerId,
    ) {
        self.clearer.destroy_layer(contexts, context_id, layer_id);
        self.layers.retain(|&ids| ids != (context_id, layer_id));
        if let Some(mut layer) = self.openxr_layers.remove(&layer_id) {
            if let Some(depth_stencil_texture) = layer.depth_stencil_texture {
                let gl = contexts.bindings(context_id).unwrap();
                unsafe { gl.delete_texture(depth_stencil_texture) };
            }
            // GPUコピー経路の裏付けを先に解放する。device/contextの借用と競合させないため先に行う。
            #[cfg(target_os = "windows")]
            if let Some(backing) = layer.copy_backing.take() {
                xr_copy::destroy_backing(contexts, context_id, backing);
            }
            let device = contexts
                .device(context_id)
                .expect("missing device for context");
            let context = contexts.context(context_id).expect("missing GL context");
            for surface_texture in mem::replace(&mut layer.surface_textures, vec![]) {
                if let Some(surface_texture) = surface_texture {
                    let mut surface = device
                        .destroy_surface_texture(context, surface_texture)
                        .unwrap();
                    device.destroy_surface(context, &mut surface).unwrap();
                }
            }
        }
    }

    fn layers(&self) -> &[(ContextId, LayerId)] {
        &self.layers[..]
    }

    fn end_frame(
        &mut self,
        contexts: &mut dyn GLContexts<SurfmanGL>,
        layers: &[(ContextId, LayerId)],
    ) -> Result<(), Error> {
        // WindowsのGPUコピー経路: 描画確定後に共有pbufferの内容をXR画像へ転送する。
        // CPU読み戻しは行わない。失敗してもpanicせず、その層の提出はstale画像のまま継続する。
        // E1では素面へ赤を描いて転送するため、contextsごと渡してbind切り替えを行う。
        #[cfg(target_os = "windows")]
        if xr_copy::copy_mode_enabled() {
            let stats = &mut self.copy_stats;
            let mut diag_submits: Vec<(u64, u32, u32, u32, u32)> = Vec::new();
            for &(context_id, layer_id) in layers {
                let Some(layer) = self.openxr_layers.get_mut(&layer_id) else {
                    continue;
                };
                let Some(xr_copy::CopyBacking::Ready(ready)) = layer.copy_backing.as_mut()
                else {
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
                if let Some(seq) = xr_copy::copy_to_swapchain(contexts, context_id, ready, dst, stats)
                {
                    if let Some((w, h, f, s)) = xr_copy::describe_image(dst) {
                        diag_submits.push((seq, w, h, f, s));
                    }
                }
            }
            stats.tick_frame();
            // 診断: 提出内容を記録する。`xr-copy-diag` の seq と対応づけて、
            // どの画像範囲をどの swapchain に出したかを見る。
            // ADR: 診断専用の観測点であり、提出経路自体は変えない。
            if !diag_submits.is_empty() {
                let guard = self.shared_data.lock().unwrap();
                if let Some(data) = guard.as_ref() {
                    let vps = data.viewports();
                    for (seq, w, h, f, s) in diag_submits {
                        let rect = |v: Rect<i32, Viewport>| {
                            serde_json::json!({
                                "x": v.origin.x,
                                "y": v.origin.y,
                                "width": v.size.width,
                                "height": v.size.height,
                            })
                        };
                        xr_copy::emit_json(serde_json::json!({
                            "event": "xr-copy-submit",
                            "seq": seq,
                            "sub_image": {
                                "image_rect": rect(vps.viewports[0]),
                                "image_array_index": 0u32,
                            },
                            "swapchain": {
                                "width": w,
                                "height": h,
                                "format": f,
                                "samples": s,
                            },
                            "viewports": vps.viewports.iter().map(|v| rect(*v)).collect::<Vec<_>>(),
                        }));
                    }
                }
            }
        }
        let guard = self.shared_data.lock().unwrap();
        let data = guard.as_ref().unwrap();

        // At this point the frame contents have been rendered, so we can release access to the texture
        // in preparation for displaying it.
        for (_, openxr_layer) in &mut self.openxr_layers {
            if openxr_layer.waited {
                openxr_layer.swapchain.release_image().map_err(|e| {
                    Error::BackendSpecific(format!("Session::release_image {:?}", e))
                })?;
                openxr_layer.waited = false;
            }
        }

        let openxr_layers = &self.openxr_layers;

        // Invert the up/down angles so that openxr flips the texture in the y axis.
        // Additionally, swap between the L/R views to compensate for inverted up/down FOVs.
        // This has no effect in runtimes that don't support fovMutable
        let mut l_fov = data.left.view.fov;
        let mut r_fov = data.right.view.fov;
        if cfg!(target_os = "windows") {
            std::mem::swap(&mut l_fov.angle_up, &mut r_fov.angle_down);
            std::mem::swap(&mut r_fov.angle_up, &mut l_fov.angle_down);
        }

        let viewports = data.viewports();
        let primary_views = layers
            .iter()
            .filter_map(|&(_, layer_id)| {
                let openxr_layer = openxr_layers.get(&layer_id)?;
                Some([
                    openxr::CompositionLayerProjectionView::new()
                        .pose(data.left.view.pose)
                        .fov(l_fov)
                        .sub_image(
                            openxr::SwapchainSubImage::new()
                                .swapchain(&openxr_layer.swapchain)
                                .image_array_index(0)
                                .image_rect(image_rect(viewports.viewports[0])),
                        ),
                    openxr::CompositionLayerProjectionView::new()
                        .pose(data.right.view.pose)
                        .fov(r_fov)
                        .sub_image(
                            openxr::SwapchainSubImage::new()
                                .swapchain(&openxr_layer.swapchain)
                                .image_array_index(0)
                                .image_rect(image_rect(viewports.viewports[1])),
                        ),
                ])
            })
            .collect::<Vec<_>>();

        let primary_layers = primary_views
            .iter()
            .map(|views| {
                CompositionLayerProjection::new()
                    .space(&data.space)
                    .layer_flags(CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA)
                    .views(&views[..])
            })
            .collect::<Vec<_>>();

        let mut primary_layers = primary_layers
            .iter()
            .map(|layer| layer.deref())
            .collect::<Vec<_>>();

        if let Some(passthrough_layer) = &self.passthrough_layer {
            let clp = CompositionLayerPassthroughFB {
                ty: CompositionLayerPassthroughFB::TYPE,
                next: std::ptr::null(),
                flags: CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA,
                space: openxr::sys::Space::default(),
                layer_handle: *passthrough_layer.inner(),
            };
            let passthrough_base = &clp as *const _ as *const CompositionLayerBase<Backend>;
            unsafe {
                primary_layers.insert(0, &*passthrough_base);
            }
        }

        if let (Some(secondary), true) = (data.secondary.as_ref(), data.secondary_active) {
            let mut s_fov = secondary.view.fov;
            std::mem::swap(&mut s_fov.angle_up, &mut s_fov.angle_down);
            let secondary_views = layers
                .iter()
                .filter_map(|&(_, layer_id)| {
                    let openxr_layer = openxr_layers.get(&layer_id)?;
                    Some([openxr::CompositionLayerProjectionView::new()
                        .pose(secondary.view.pose)
                        .fov(s_fov)
                        .sub_image(
                            openxr::SwapchainSubImage::new()
                                .swapchain(&openxr_layer.swapchain)
                                .image_array_index(0)
                                .image_rect(image_rect(viewports.viewports[2])),
                        )])
                })
                .collect::<Vec<_>>();

            let secondary_layers = secondary_views
                .iter()
                .map(|views| {
                    CompositionLayerProjection::new()
                        .space(&data.space)
                        .layer_flags(CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA)
                        .views(&views[..])
                })
                .collect::<Vec<_>>();

            let secondary_layers = secondary_layers
                .iter()
                .map(|layer| layer.deref())
                .collect::<Vec<_>>();

            let end_result = self
                .frame_stream
                .end_secondary(
                    data.frame_state.as_ref().unwrap().predicted_display_time,
                    data.primary_blend_mode,
                    &primary_layers[..],
                    SecondaryEndInfo {
                        ty: ViewConfigurationType::SECONDARY_MONO_FIRST_PERSON_OBSERVER_MSFT,
                        // XXXManishearth should we use the secondary layer's blend mode here, given
                        // that the content will be using the primary blend mode?
                        environment_blend_mode: data
                            .secondary_blend_mode
                            .unwrap_or(data.primary_blend_mode),
                        layers: &secondary_layers[..],
                    },
                )
                .map_err(|e| {
                    Error::BackendSpecific(format!("FrameStream::end_secondary {:?}", e))
                });
            #[cfg(target_os = "windows")]
            if end_result.is_err() {
                self.copy_stats.end_failures += 1;
            }
            end_result?;
        } else {
            let end_result = self
                .frame_stream
                .end(
                    data.frame_state.as_ref().unwrap().predicted_display_time,
                    data.primary_blend_mode,
                    &primary_layers[..],
                )
                .map_err(|e| Error::BackendSpecific(format!("FrameStream::end {:?}", e)));
            #[cfg(target_os = "windows")]
            if end_result.is_err() {
                self.copy_stats.end_failures += 1;
            }
            end_result?;
        }
        Ok(())
    }

    fn begin_frame(
        &mut self,
        contexts: &mut dyn GLContexts<SurfmanGL>,
        layers: &[(ContextId, LayerId)],
    ) -> Result<Vec<SubImages>, Error> {
        let data_guard = self.shared_data.lock().unwrap();
        let data = data_guard.as_ref().unwrap();
        let openxr_layers = &mut self.openxr_layers;
        let clearer = &mut self.clearer;
        #[cfg(target_os = "windows")]
        let stats = &mut self.copy_stats;
        self.frame_stream
            .begin()
            .map_err(|e| {
                error!("[xr-debug] FrameStream::begin failed: {:?}", e);
                Error::BackendSpecific(format!("FrameStream::begin {:?}", e))
            })?;
        layers
            .iter()
            .map(|&(context_id, layer_id)| {
                let device = contexts.device(context_id).ok_or_else(|| {
                    error!("[xr-debug] no device for context {context_id:?}");
                    Error::NoMatchingDevice
                })?;
                let context = contexts.context(context_id).ok_or_else(|| {
                    error!("[xr-debug] no surfman context for {context_id:?}");
                    Error::NoMatchingDevice
                })?;
                let openxr_layer = openxr_layers.get_mut(&layer_id).ok_or_else(|| {
                    error!("[xr-debug] no openxr layer for {layer_id:?}");
                    Error::NoMatchingDevice
                })?;

                let image = match openxr_layer.swapchain.acquire_image() {
                    Ok(image) => image,
                    Err(e) => {
                        error!("[xr-debug] acquire_image failed: {:?}", e);
                        return Err(Error::BackendSpecific(format!(
                            "Swapchain::acquire_image {:?}",
                            e
                        )));
                    },
                };
                // GPUコピー経路の層は共有pbufferへ描画する。XR画像への転送はend_frameで行う。
                let use_copy = {
                    #[cfg(target_os = "windows")]
                    {
                        openxr_layer.copy_backing.is_some()
                    }
                    #[cfg(not(target_os = "windows"))]
                    {
                        false
                    }
                };
                let (color_texture, color_target) = if use_copy {
                    #[cfg(target_os = "windows")]
                    {
                        if matches!(
                            openxr_layer.copy_backing,
                            Some(xr_copy::CopyBacking::Broken)
                        ) {
                            error!("[xr-debug] copy backing is broken; skipping frame");
                            return Err(Error::BackendSpecific(
                                "Layer GPU-copy backing is broken".into(),
                            ));
                        }
                        // copy_backingとswapchainは別フィールドのため同時借用できる。
                        let ready = match openxr_layer.copy_backing.as_mut() {
                            Some(xr_copy::CopyBacking::Ready(ready)) => ready,
                            // 直上でBrokenを弾いているため到達しない。
                            _ => {
                                error!("[xr-debug] copy backing vanished; skipping frame");
                                return Err(Error::BackendSpecific(
                                    "Layer GPU-copy backing vanished".into(),
                                ));
                            },
                        };
                        if let Err(e) =
                            xr_copy::wait_for_copy(&mut openxr_layer.swapchain, ready, image)
                        {
                            stats.note_wait_failure();
                            error!("[xr-debug] wait_image failed: {:?}", e);
                            return Err(e);
                        }
                        openxr_layer.waited = true;
                        // E2-blit既定では自前の通常テクスチャへ描く。旧E2の取り込み面は使わない。
                        // ページは今まで通り `XRWebGLLayer.framebuffer` に描き、end_frameでblitする。
                        if let Some(page) = ready.page_texture {
                            (Some(page), ready.page_target)
                        } else {
                            (Some(ready.gl_texture), ready.gl_target)
                        }
                    }
                    #[cfg(not(target_os = "windows"))]
                    {
                        // use_copyは非Windowsで常にfalseのため到達しない。
                        unreachable!("GPU copy path is Windows-only");
                    }
                } else {
                    if let Err(e) = openxr_layer
                        .swapchain
                        .wait_image(openxr::Duration::INFINITE)
                    {
                        error!("[xr-debug] wait_image failed: {:?}", e);
                        return Err(Error::BackendSpecific(format!(
                            "Swapchain::wait_image {:?}",
                            e
                        )));
                    }
                    openxr_layer.waited = true;

                    let color_surface_texture =
                        match openxr_layer.get_surface_texture(&device, context, image as usize) {
                            Ok(texture) => texture,
                            Err(e) => {
                                error!("[xr-debug] get_surface_texture failed: {:?}", e);
                                return Err(Error::BackendSpecific(format!(
                                    "Layer::get_surface_texture {:?}",
                                    e
                                )));
                            },
                        };
                    let color_texture = device.surface_texture_object(color_surface_texture);
                    let color_target = device.surface_gl_texture_target();
                    (color_texture, color_target)
                };
                let depth_stencil_texture = openxr_layer.depth_stencil_texture;
                let texture_array_index = None;
                let origin = Point2D::new(0, 0);
                let texture_size = openxr_layer.size;
                let sub_image = Some(SubImage {
                    color_texture: color_texture.map(|t| t.0),
                    depth_stencil_texture: depth_stencil_texture.map(|t| t.0),
                    texture_array_index,
                    viewport: Rect::new(origin, texture_size),
                });
                let view_sub_images = data
                    .viewports()
                    .viewports
                    .iter()
                    .map(|&viewport| SubImage {
                        color_texture: color_texture.map(|t| t.0),
                        depth_stencil_texture: depth_stencil_texture.map(|t| t.0),
                        texture_array_index,
                        viewport,
                    })
                    .collect();
                clearer.clear(
                    contexts,
                    context_id,
                    layer_id,
                    color_texture,
                    color_target,
                    openxr_layer.depth_stencil_texture,
                );
                Ok(SubImages {
                    layer_id,
                    sub_image,
                    view_sub_images,
                })
            })
            .collect()
    }
}

fn image_rect(viewport: Rect<i32, Viewport>) -> openxr::Rect2Di {
    openxr::Rect2Di {
        extent: openxr::Extent2Di {
            height: viewport.size.height,
            width: viewport.size.width,
        },
        offset: openxr::Offset2Di {
            x: viewport.origin.x,
            y: viewport.origin.y,
        },
    }
}

impl OpenXrDevice {
    fn new(
        instance: CreatedInstance,
        granted_features: Vec<String>,
        context_menu_provider: Option<Box<dyn ContextMenuProvider>>,
        grand_manager: LayerGrandManager<SurfmanGL>,
    ) -> Result<OpenXrDevice, Error> {
        let CreatedInstance {
            instance,
            supports_hands,
            supports_secondary,
            system,
            supports_mutable_fov,
            supported_interaction_profiles,
            supports_passthrough,
            supports_updating_framerate,
        } = instance;

        let (init_tx, init_rx) = crossbeam_channel::unbounded();

        let instance_clone = instance.clone();
        let shared_data = Arc::new(Mutex::new(None));
        let shared_data_clone = shared_data.clone();
        let mut data = shared_data.lock().unwrap();

        let layer_manager = grand_manager.create_layer_manager(move |_| {
            let (session, frame_waiter, frame_stream) =
                GraphicsProvider::create_session(&instance_clone, system)?;
            let (passthrough, passthrough_layer) = if supports_passthrough {
                let flags = PassthroughFlagsFB::IS_RUNNING_AT_CREATION;
                let purpose = PassthroughLayerPurposeFB::RECONSTRUCTION;
                let passthrough = session
                    .create_passthrough(flags)
                    .expect("Unable to initialize passthrough");
                let passthrough_layer = session
                    .create_passthrough_layer(&passthrough, flags, purpose)
                    .expect("Failed to create passthrough layer");
                (Some(passthrough), Some(passthrough_layer))
            } else {
                (None, None)
            };
            let session = Arc::new(session);
            init_tx
                .send((session.clone(), frame_waiter))
                .map_err(|_| Error::CommunicationError)?;
            Ok(OpenXrLayerManager::new(
                session,
                shared_data_clone,
                frame_stream,
                !supports_mutable_fov,
                passthrough,
                passthrough_layer,
            ))
        })?;

        let (session, frame_waiter) = init_rx.recv().map_err(|_| Error::CommunicationError)?;

        // Wait for the runtime to report READY before calling begin (some runtimes reject an immediate begin).
        let mut ready = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let mut buffer = openxr::EventDataBuffer::new();
            match instance.poll_event(&mut buffer) {
                Ok(Some(openxr::Event::SessionStateChanged(sc))) => match sc.state() {
                    openxr::SessionState::READY
                    | openxr::SessionState::SYNCHRONIZED
                    | openxr::SessionState::VISIBLE
                    | openxr::SessionState::FOCUSED => {
                        ready = true;
                        break;
                    },
                    openxr::SessionState::STOPPING
                    | openxr::SessionState::EXITING
                    | openxr::SessionState::LOSS_PENDING => {
                        break;
                    },
                    _ => {},
                },
                Ok(_) => {},
                Err(_) => {},
            }
            thread::sleep(Duration::from_millis(5));
        }
        if ready {
            info!("OpenXR session reached READY before begin");
        } else {
            warn!("OpenXR session did not reach READY before begin (timeout or terminal state)");
        }

        if supports_secondary {
            if let Err(e) = session.begin_with_secondary(
                ViewConfigurationType::PRIMARY_STEREO,
                &[ViewConfigurationType::SECONDARY_MONO_FIRST_PERSON_OBSERVER_MSFT],
            ) {
                error!(
                    "Session failed to begin after READY wait (ready={}): {:?}",
                    ready, e
                );
            }
        } else if let Err(e) = session.begin(ViewConfigurationType::PRIMARY_STEREO) {
            error!(
                "Session failed to begin after READY wait (ready={}): {:?}",
                ready, e
            );
        }

        let pose = Posef {
            orientation: Quaternionf {
                x: 0.,
                y: 0.,
                z: 0.,
                w: 1.,
            },
            position: Vector3f {
                x: 0.,
                y: 0.,
                z: 0.,
            },
        };
        let space = session
            .create_reference_space(ReferenceSpaceType::LOCAL, pose)
            .map_err(|e| {
                Error::BackendSpecific(format!("Session::create_reference_space {:?}", e))
            })?;

        let viewer_space = session
            .create_reference_space(ReferenceSpaceType::VIEW, pose)
            .map_err(|e| {
                Error::BackendSpecific(format!("Session::create_reference_space {:?}", e))
            })?;

        let view_configuration_type = ViewConfigurationType::PRIMARY_STEREO;
        let view_configurations = instance
            .enumerate_view_configuration_views(system, view_configuration_type)
            .map_err(|e| {
                Error::BackendSpecific(format!(
                    "Session::enumerate_view_configuration_views {:?}",
                    e
                ))
            })?;

        let left_view_configuration = view_configurations[0];
        let right_view_configuration = view_configurations[1];
        let left_extent = Extent2Di {
            width: left_view_configuration.recommended_image_rect_width as i32,
            height: left_view_configuration.recommended_image_rect_height as i32,
        };
        let right_extent = Extent2Di {
            width: right_view_configuration.recommended_image_rect_width as i32,
            height: right_view_configuration.recommended_image_rect_height as i32,
        };

        assert_eq!(
            left_view_configuration.recommended_image_rect_height,
            right_view_configuration.recommended_image_rect_height,
        );

        let swapchain_sample_count = left_view_configuration.recommended_swapchain_sample_count;

        let secondary_active = false;
        let (secondary, secondary_blend_mode) = if supports_secondary {
            let view_configuration = *instance
                .enumerate_view_configuration_views(
                    system,
                    ViewConfigurationType::SECONDARY_MONO_FIRST_PERSON_OBSERVER_MSFT,
                )
                .map_err(|e| {
                    Error::BackendSpecific(format!(
                        "Session::enumerate_view_configuration_views {:?}",
                        e
                    ))
                })?
                .get(0)
                .expect(
                    "Session::enumerate_view_configuration_views() returned no secondary views",
                );

            let secondary_blend_mode = instance
                .enumerate_environment_blend_modes(
                    system,
                    ViewConfigurationType::SECONDARY_MONO_FIRST_PERSON_OBSERVER_MSFT,
                )
                .map_err(|e| {
                    Error::BackendSpecific(format!(
                        "Instance::enumerate_environment_blend_modes {:?}",
                        e
                    ))
                })?[0];

            let secondary_extent = Extent2Di {
                width: view_configuration.recommended_image_rect_width as i32,
                height: view_configuration.recommended_image_rect_height as i32,
            };

            let secondary = ViewInfo {
                view: VIEW_INIT,
                extent: secondary_extent,
                cached_projection: Transform3D::identity(),
            };

            (Some(secondary), Some(secondary_blend_mode))
        } else {
            (None, None)
        };

        let primary_blend_mode = instance
            .enumerate_environment_blend_modes(system, view_configuration_type)
            .map_err(|e| {
                Error::BackendSpecific(format!(
                    "Instance::enumerate_environment_blend_modes {:?}",
                    e
                ))
            })?[0];

        let left = ViewInfo {
            view: VIEW_INIT,
            extent: left_extent,
            cached_projection: Transform3D::identity(),
        };
        let right = ViewInfo {
            view: VIEW_INIT,
            extent: right_extent,
            cached_projection: Transform3D::identity(),
        };
        *data = Some(SharedData {
            frame_state: None,
            space,
            left,
            right,
            secondary,
            secondary_active,
            primary_blend_mode,
            secondary_blend_mode,
            swapchain_sample_count,
        });
        drop(data);

        let (action_set, right_hand, left_hand) = OpenXRInput::setup_inputs(
            &instance,
            &session,
            supports_hands,
            supported_interaction_profiles,
        );

        Ok(OpenXrDevice {
            instance,
            events: Default::default(),
            session,
            frame_waiter,
            viewer_space,
            clip_planes: Default::default(),
            supports_secondary,
            supports_mutable_fov,
            supports_updating_framerate,
            layer_manager,
            shared_data,

            action_set,
            right_hand,
            left_hand,
            granted_features,
            context_menu_provider,
            context_menu_future: None,
        })
    }

    fn handle_openxr_events(&mut self) -> bool {
        use openxr::Event::*;
        let mut stopped = false;
        loop {
            let mut buffer = openxr::EventDataBuffer::new();
            let event = match self.instance.poll_event(&mut buffer) {
                Ok(event) => event,
                Err(e) => {
                    error!("Error polling events: {:?}", e);
                    return false;
                },
            };
            match event {
                Some(SessionStateChanged(session_change)) => match session_change.state() {
                    openxr::SessionState::EXITING | openxr::SessionState::LOSS_PENDING => {
                        self.events.callback(Event::SessionEnd);
                        return false;
                    },
                    openxr::SessionState::STOPPING => {
                        self.events
                            .callback(Event::VisibilityChange(Visibility::Hidden));
                        if let Err(e) = self.session.end() {
                            error!("Session failed to end on STOPPING: {:?}", e);
                        }
                        stopped = true;
                    },
                    openxr::SessionState::READY if stopped => {
                        self.events
                            .callback(Event::VisibilityChange(Visibility::Visible));
                        if let Err(e) = self.session.begin(ViewConfigurationType::PRIMARY_STEREO) {
                            error!("Session failed to begin on READY: {:?}", e);
                        }
                        stopped = false;
                    },
                    openxr::SessionState::FOCUSED => {
                        self.events
                            .callback(Event::VisibilityChange(Visibility::Visible));
                    },
                    openxr::SessionState::VISIBLE => {
                        self.events
                            .callback(Event::VisibilityChange(Visibility::VisibleBlurred));
                    },
                    _ => {
                        // FIXME: Handle other states
                    },
                },
                Some(InstanceLossPending(_)) => {
                    self.events.callback(Event::SessionEnd);
                    return false;
                },
                Some(InteractionProfileChanged(_)) => {
                    let path = self.instance.string_to_path("/user/hand/right").unwrap();
                    let profile_path = self.session.current_interaction_profile(path).unwrap();
                    let profile = self.instance.path_to_string(profile_path);

                    match profile {
                        Ok(profile) => {
                            let profiles = get_profiles_from_path(profile)
                                .iter()
                                .map(|s| s.to_string())
                                .collect();

                            let mut new_left = self.left_hand.input_source();
                            new_left.profiles.clone_from(&profiles);
                            self.events
                                .callback(Event::UpdateInput(new_left.id, new_left));

                            let mut new_right = self.right_hand.input_source();
                            new_right.profiles.clone_from(&profiles);
                            self.events
                                .callback(Event::UpdateInput(new_right.id, new_right));
                        },
                        Err(e) => {
                            error!("Failed to get interaction profile: {:?}", e);
                        },
                    }
                },
                Some(ReferenceSpaceChangePending(e)) => {
                    let base_space = match e.reference_space_type() {
                        ReferenceSpaceType::VIEW => BaseSpace::Viewer,
                        ReferenceSpaceType::LOCAL => BaseSpace::Local,
                        ReferenceSpaceType::LOCAL_FLOOR => BaseSpace::Floor,
                        ReferenceSpaceType::STAGE => BaseSpace::BoundedFloor,
                        _ => unreachable!(
                            "Should not be receiving change events for unsupported space types"
                        ),
                    };
                    let transform = transform(&e.pose_in_previous_space());
                    self.events
                        .callback(Event::ReferenceSpaceChanged(base_space, transform));
                },
                Some(_) => {
                    // FIXME: Handle other events
                },
                None if stopped => {
                    // XXXManishearth be able to handle exits during this time
                    thread::sleep(Duration::from_millis(200));
                },
                None => {
                    // No more events to process
                    break;
                },
            }
        }
        true
    }
}

impl SharedData {
    fn views(&self) -> Views {
        let left_view = self.left.view();
        let right_view = self.right.view();
        if let (Some(secondary), true) = (self.secondary.as_ref(), self.secondary_active) {
            // Note: we report the secondary view only when it is active
            let third_eye = secondary.view();
            return Views::StereoCapture(left_view, right_view, third_eye);
        }
        Views::Stereo(left_view, right_view)
    }

    fn viewports(&self) -> Viewports {
        let left_vp = Rect::new(
            Point2D::zero(),
            Size2D::new(self.left.extent.width, self.left.extent.height),
        );
        let right_vp = Rect::new(
            Point2D::new(self.left.extent.width, 0),
            Size2D::new(self.right.extent.width, self.right.extent.height),
        );
        let mut viewports = vec![left_vp, right_vp];
        // Note: we report the secondary viewport even when it is inactive
        if let Some(ref secondary) = self.secondary {
            let secondary_vp = Rect::new(
                Point2D::new(self.left.extent.width + self.right.extent.width, 0),
                Size2D::new(secondary.extent.width, secondary.extent.height) /
                    SECONDARY_VIEW_DOWNSCALE,
            );
            viewports.push(secondary_vp)
        }
        Viewports { viewports }
    }
}

impl DeviceAPI for OpenXrDevice {
    fn floor_transform(&self) -> Option<RigidTransform3D<f32, Native, Floor>> {
        let translation = Vector3D::new(0.0, HEIGHT, 0.0);
        Some(RigidTransform3D::from_translation(translation))
    }

    fn viewports(&self) -> Viewports {
        self.shared_data
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .viewports()
    }

    fn create_layer(&mut self, context_id: ContextId, init: LayerInit) -> Result<LayerId, Error> {
        self.layer_manager.create_layer(context_id, init)
    }

    fn destroy_layer(&mut self, context_id: ContextId, layer_id: LayerId) {
        self.layer_manager.destroy_layer(context_id, layer_id)
    }

    fn begin_animation_frame(&mut self, layers: &[(ContextId, LayerId)]) -> Option<Frame> {
        if !self.handle_openxr_events() {
            warn!("no frame, session isn't running");
            // Session is not running anymore.
            return None;
        }
        if let Some(ref context_menu_future) = self.context_menu_future {
            match context_menu_future.poll() {
                ContextMenuResult::ExitSession => {
                    self.quit();
                    return None;
                },
                ContextMenuResult::Dismissed => self.context_menu_future = None,
                ContextMenuResult::Pending => (),
            }
        }

        let (frame_state, secondary_state) = if self.supports_secondary {
            let (frame_state, secondary_state) = match self.frame_waiter.wait_secondary() {
                Ok(frame_state) => frame_state,
                Err(e) => {
                    error!("Error waiting on frame: {:?}", e);
                    return None;
                },
            };

            assert_eq!(
                secondary_state.ty,
                ViewConfigurationType::SECONDARY_MONO_FIRST_PERSON_OBSERVER_MSFT
            );
            (frame_state, Some(secondary_state))
        } else {
            match self.frame_waiter.wait() {
                Ok(frame_state) => (frame_state, None),
                Err(e) => {
                    error!("Error waiting on frame: {:?}", e);
                    return None;
                },
            }
        };

        // We get the subimages before grabbing the lock,
        // since otherwise we'll deadlock
        let sub_images = match self.layer_manager.begin_frame(layers) {
            Ok(sub_images) => sub_images,
            Err(e) => {
                error!("LayerManager::begin_frame failed: {:?}", e);
                return None;
            },
        };

        let mut guard = self.shared_data.lock().unwrap();
        let data = guard.as_mut().unwrap();

        // XXXManishearth should we check frame_state.should_render?
        let (_view_flags, mut views) = match self.session.locate_views(
            ViewConfigurationType::PRIMARY_STEREO,
            frame_state.predicted_display_time,
            &data.space,
        ) {
            Ok(data) => data,
            Err(e) => {
                error!("Error locating views: {:?}", e);
                return None;
            },
        };
        if !self.supports_mutable_fov {
            views.iter_mut().for_each(|v| {
                std::mem::swap(&mut v.fov.angle_up, &mut v.fov.angle_down);
            });
        }
        data.left.set_view(views[0], self.clip_planes);
        data.right.set_view(views[1], self.clip_planes);
        let pose = match self
            .viewer_space
            .locate(&data.space, frame_state.predicted_display_time)
        {
            Ok(pose) => pose,
            Err(e) => {
                error!("Error locating viewer space: {:?}", e);
                return None;
            },
        };
        let transform = transform(&pose.pose);

        if let Some(secondary_state) = secondary_state.as_ref() {
            data.secondary_active = secondary_state.active;
        }
        if let (Some(secondary), true) = (data.secondary.as_mut(), data.secondary_active) {
            let view = match self.session.locate_views(
                ViewConfigurationType::SECONDARY_MONO_FIRST_PERSON_OBSERVER_MSFT,
                frame_state.predicted_display_time,
                &data.space,
            ) {
                Ok(v) => v.1[0],
                Err(e) => {
                    error!("Error locating views: {:?}", e);
                    return None;
                },
            };
            secondary.set_view(view, self.clip_planes);
        }

        let active_action_set = ActiveActionSet::new(&self.action_set);

        if let Err(e) = self.session.sync_actions(&[active_action_set]) {
            error!("Error syncing actions: {:?}", e);
            return None;
        }

        let mut right = self
            .right_hand
            .frame(&self.session, &frame_state, &data.space, &transform);
        let mut left = self
            .left_hand
            .frame(&self.session, &frame_state, &data.space, &transform);

        data.frame_state = Some(frame_state);
        let views = data.views();

        if let Some(ref context_menu_provider) = self.context_menu_provider {
            if (left.menu_selected || right.menu_selected) && self.context_menu_future.is_none() {
                self.context_menu_future = Some(context_menu_provider.open_context_menu());
            } else if self.context_menu_future.is_some() {
                // Do not surface input info whilst the context menu is open
                // We don't do this for the first frame after the context menu is opened
                // so that the appropriate select cancel events may fire
                right.frame.target_ray_origin = None;
                right.frame.grip_origin = None;
                left.frame.target_ray_origin = None;
                left.frame.grip_origin = None;
                right.select = None;
                right.squeeze = None;
                left.select = None;
                left.squeeze = None;
            }
        }

        let left_input_changed = left.frame.input_changed;
        let right_input_changed = right.frame.input_changed;

        let frame = Frame {
            pose: Some(ViewerPose { transform, views }),
            inputs: vec![right.frame, left.frame],
            events: vec![],
            sub_images,
            hit_test_results: vec![],
            predicted_display_time: frame_state.predicted_display_time.as_nanos() as f64,
        };

        if let Some(right_select) = right.select {
            self.events.callback(Event::Select(
                InputId(0),
                SelectKind::Select,
                right_select,
                frame.clone(),
            ));
        }
        if let Some(right_squeeze) = right.squeeze {
            self.events.callback(Event::Select(
                InputId(0),
                SelectKind::Squeeze,
                right_squeeze,
                frame.clone(),
            ));
        }
        if let Some(left_select) = left.select {
            self.events.callback(Event::Select(
                InputId(1),
                SelectKind::Select,
                left_select,
                frame.clone(),
            ));
        }
        if let Some(left_squeeze) = left.squeeze {
            self.events.callback(Event::Select(
                InputId(1),
                SelectKind::Squeeze,
                left_squeeze,
                frame.clone(),
            ));
        }
        if left_input_changed {
            self.events
                .callback(Event::InputChanged(InputId(1), frame.inputs[1].clone()))
        }
        if right_input_changed {
            self.events
                .callback(Event::InputChanged(InputId(0), frame.inputs[0].clone()))
        }
        Some(frame)
    }

    fn end_animation_frame(&mut self, layers: &[(ContextId, LayerId)]) {
        // We tell OpenXR to display the frame in the layer manager.
        // Due to threading issues we can't call D3D11 APIs on the openxr thread as the
        // WebGL thread might be using the device simultaneously, so this method delegates
        // everything to the layer manager.
        let _ = self.layer_manager.end_frame(layers);
    }

    fn initial_inputs(&self) -> Vec<InputSource> {
        vec![
            self.right_hand.input_source(),
            self.left_hand.input_source(),
        ]
    }

    fn set_event_dest(&mut self, dest: ProfileGenericCallback<Event>) {
        self.events.upgrade(dest)
    }

    fn quit(&mut self) {
        self.session.request_exit().unwrap();
        loop {
            let mut buffer = openxr::EventDataBuffer::new();
            let event = match self.instance.poll_event(&mut buffer) {
                Ok(e) => e,
                Err(e) => {
                    error!("Error polling for event while quitting: {:?}", e);
                    break;
                },
            };
            match event {
                Some(openxr::Event::SessionStateChanged(session_change)) => {
                    match session_change.state() {
                        openxr::SessionState::EXITING => {
                            break;
                        },
                        openxr::SessionState::STOPPING => {
                            if let Err(e) = self.session.end() {
                                error!("Session failed to end while STOPPING: {:?}", e);
                            }
                        },
                        _ => (),
                    }
                },
                _ => (),
            }
            thread::sleep(Duration::from_millis(30));
        }
        self.events.callback(Event::SessionEnd);
        // We clear this data to remove the outstanding reference to XrSpace,
        // which keeps other OpenXR objects alive.
        *self.shared_data.lock().unwrap() = None;
    }

    fn set_quitter(&mut self, _: Quitter) {
        // the quitter is only needed if we have anything from outside the render
        // thread that can signal a quit. We don't.
    }

    fn update_clip_planes(&mut self, near: f32, far: f32) {
        self.clip_planes.update(near, far);
    }

    fn environment_blend_mode(&self) -> webxr_api::EnvironmentBlendMode {
        match self
            .shared_data
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .primary_blend_mode
        {
            EnvironmentBlendMode::OPAQUE => webxr_api::EnvironmentBlendMode::Opaque,
            EnvironmentBlendMode::ALPHA_BLEND => webxr_api::EnvironmentBlendMode::AlphaBlend,
            EnvironmentBlendMode::ADDITIVE => webxr_api::EnvironmentBlendMode::Additive,
            v => unimplemented!("unsupported blend mode: {:?}", v),
        }
    }

    fn granted_features(&self) -> &[String] {
        &self.granted_features
    }

    fn update_frame_rate(&mut self, rate: f32) -> f32 {
        if self.supports_updating_framerate {
            self.session
                .request_display_refresh_rate(rate)
                .expect("Failed to request display refresh rate");
            self.session
                .get_display_refresh_rate()
                .expect("Failed to get display refresh rate")
        } else {
            -1.0
        }
    }

    fn supported_frame_rates(&self) -> Vec<f32> {
        if self.supports_updating_framerate {
            self.session
                .enumerate_display_refresh_rates()
                .expect("Failed to enumerate display refresh rates")
        } else {
            vec![]
        }
    }

    fn reference_space_bounds(&self) -> Option<Vec<Point2D<f32, Floor>>> {
        match self
            .session
            .reference_space_bounds_rect(ReferenceSpaceType::STAGE)
        {
            Ok(bounds) => {
                if let Some(bounds) = bounds {
                    let point1 = Point2D::new(-bounds.width / 2., -bounds.height / 2.);
                    let point2 = Point2D::new(-bounds.width / 2., bounds.height / 2.);
                    let point3 = Point2D::new(bounds.width / 2., bounds.height / 2.);
                    let point4 = Point2D::new(bounds.width / 2., -bounds.height / 2.);
                    Some(vec![point1, point2, point3, point4])
                } else {
                    None
                }
            },
            Err(_) => None,
        }
    }
}

fn transform<Src, Dst>(pose: &Posef) -> RigidTransform3D<f32, Src, Dst> {
    let rotation = Rotation3D::quaternion(
        pose.orientation.x,
        pose.orientation.y,
        pose.orientation.z,
        pose.orientation.w,
    );
    let translation = Vector3D::new(pose.position.x, pose.position.y, pose.position.z);
    RigidTransform3D::new(rotation, translation)
}

#[inline]
fn fov_to_projection_matrix<T, U>(fov: &Fovf, clip_planes: ClipPlanes) -> Transform3D<f32, T, U> {
    util::fov_to_projection_matrix(
        fov.angle_left,
        fov.angle_right,
        fov.angle_up,
        fov.angle_down,
        clip_planes,
    )
}

/// Servo(ANGLE/GL)の描画結果を別D3D11デバイスへ渡せるかを判定するための能力計測。
///
/// 画素の読み戻しはしない。`OpenXrLayerManager::create_layer`が最初に呼ばれた1回だけ
/// 小さなpbuffer共有ハンドルを作り、XRアダプタ上のD3D11デバイスで開けるかを記録する。
/// どこで失敗してもpanicさせず、各段階の成否を1行JSONで残して先へ進む。
/// 計測後は削除予定のため、このモジュールに集約する。
#[cfg(target_os = "windows")]
mod xr_probe {
    use std::sync::atomic::{AtomicBool, Ordering};

    use surfman::{SurfaceAccess, SurfaceType};

    use super::graphics_d3d11::Backend;
    use crate::SurfmanGL;

    static DONE: AtomicBool = AtomicBool::new(false);

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
        log::info!("[xr-probe] {line}");
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
}
