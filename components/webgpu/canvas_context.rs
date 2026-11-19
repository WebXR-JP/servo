/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Main process implementation of [GPUCanvasContext](https://www.w3.org/TR/webgpu/#canvas-context)

use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use arrayvec::ArrayVec;
use euclid::default::Size2D;
use log::warn;
use paint_api::{
    CrossProcessPaintApi, ExternalImageSource, SerializableImageData, WebRenderExternalImageApi,
};
use pixels::{SharedSnapshot, Snapshot, SnapshotAlphaMode, SnapshotPixelFormat};
use rustc_hash::FxHashMap;
use servo_base::Epoch;
use servo_base::generic_channel::{GenericSender, GenericSharedMemory};
use webgpu_traits::id::{
    self, BufferId, CommandBufferId, CommandEncoderId, DeviceId, QueueId, TextureId,
};
use webgpu_traits::{
    BufferDescriptor, BufferUsages, CommandBufferDescriptor, CommandEncoderDescriptor,
    ContextConfiguration, Extent3d, HostMap, Origin3d, PRESENTATION_BUFFER_COUNT, PendingTexture,
    TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, TextureAspect,
    WebGPUContextId, WebGPUMsg,
};
use webrender_api::units::DeviceIntSize;
use webrender_api::{ImageDescriptor, ImageDescriptorFlags, ImageFormat, ImageKey};
use wgpu_core::global::Global;
use wgpu_core::resource::{BufferAccessError, BufferMapOperation, CreateBufferError};
use wgpu_types::COPY_BYTES_PER_ROW_ALIGNMENT;

pub type WebGpuExternalImageMap = Arc<Mutex<FxHashMap<WebGPUContextId, ContextData>>>;

/// Builds the [`ImageDescriptor`] and [`SerializableImageData`] pair for a
/// mapped presentation buffer, so the descriptor and the pixel bytes travel to
/// WebRender in a single atomic `add_image`/`update_image` transaction.
fn mapped_descriptor_and_data(mapped: &MappedBuffer) -> (ImageDescriptor, SerializableImageData) {
    let descriptor = ImageDescriptor {
        format: mapped.image_format,
        size: mapped.image_size.cast().cast_unit(),
        stride: Some(mapped.stride() as i32),
        offset: 0,
        flags: if mapped.is_opaque {
            ImageDescriptorFlags::IS_OPAQUE
        } else {
            ImageDescriptorFlags::empty()
        },
    };
    let data = SerializableImageData::Raw(GenericSharedMemory::from_bytes(mapped.slice()));
    (descriptor, data)
}

/// Builds a zero-filled [`ImageDescriptor`] and [`SerializableImageData`] pair
/// for the given canvas size.
///
/// WebRender rejects empty image sizes with an assert, so the size is clamped
/// to at least 1x1 (same convention as `MIN_WR_IMAGE_SIZE` in `canvas_data.rs`).
fn cleared_descriptor_and_data(size: DeviceIntSize) -> (ImageDescriptor, SerializableImageData) {
    let size = DeviceIntSize::new(size.width.max(1), size.height.max(1));
    let descriptor = ImageDescriptor {
        format: ImageFormat::BGRA8,
        size,
        stride: None,
        offset: 0,
        flags: ImageDescriptorFlags::empty(),
    };
    let data = SerializableImageData::Raw(GenericSharedMemory::from_byte(
        0,
        size.width as usize * size.height as usize * 4,
    ));
    (descriptor, data)
}

/// Returns the [`ImageDescriptor`] and [`SerializableImageData`] pair
/// describing the currently adopted presentation, or a cleared image of the
/// current canvas size when there is no (mapped) presentation.
fn current_descriptor_and_data(
    context_data: &ContextData,
) -> (ImageDescriptor, SerializableImageData) {
    if let Some(presentation) = context_data.presentation.as_ref() {
        if let StagingBufferState::Mapped(mapped) = &presentation.staging_buffer.state {
            return mapped_descriptor_and_data(mapped);
        }
    }
    cleared_descriptor_and_data(context_data.size)
}

/// Allocated buffer on GPU device
#[derive(Clone, Copy, Debug)]
struct Buffer {
    device_id: DeviceId,
    queue_id: QueueId,
    size: u64,
}

impl Buffer {
    /// Returns true if buffer is compatible with provided configuration
    fn has_compatible_config(&self, config: &ContextConfiguration) -> bool {
        config.device_id == self.device_id && self.size == config.buffer_size()
    }
}

/// Mapped GPUBuffer
#[derive(Debug)]
struct MappedBuffer {
    buffer: Buffer,
    data: NonNull<u8>,
    len: u64,
    image_size: Size2D<u32>,
    image_format: ImageFormat,
    is_opaque: bool,
}

// Mapped buffer can be shared between safely (it's read-only)
unsafe impl Send for MappedBuffer {}

impl MappedBuffer {
    const fn slice(&'_ self) -> &'_ [u8] {
        // Safety: Pointer is from wgpu, and we only use it here
        unsafe { std::slice::from_raw_parts(self.data.as_ptr(), self.len as usize) }
    }

    fn stride(&self) -> u32 {
        (self.image_size.width * self.image_format.bytes_per_pixel() as u32)
            .next_multiple_of(COPY_BYTES_PER_ROW_ALIGNMENT)
    }
}

#[derive(Debug)]
enum StagingBufferState {
    /// The Initial state: the buffer has yet to be created with only an
    /// id reserved for it.
    Unassigned,
    /// The buffer is allocated in the WGPU Device and is ready to be used.
    Available(Buffer),
    /// `mapAsync` is currently running on the buffer.
    Mapping(Buffer),
    /// The buffer is currently mapped.
    Mapped(MappedBuffer),
}

/// A staging buffer used for texture to buffer to CPU copy operations.
#[derive(Debug)]
struct StagingBuffer {
    global: Arc<Global>,
    buffer_id: BufferId,
    state: StagingBufferState,
}

// [`StagingBuffer`] only used for reading (never for writing)
// so it is safe to share between threads.
unsafe impl Sync for StagingBuffer {}

impl StagingBuffer {
    fn new(global: Arc<Global>, buffer_id: BufferId) -> Self {
        Self {
            global,
            buffer_id,
            state: StagingBufferState::Unassigned,
        }
    }

    const fn is_mapped(&self) -> bool {
        matches!(self.state, StagingBufferState::Mapped(..))
    }

    /// Return true if buffer can be used directly with provided config
    /// without any additional work
    fn is_available_and_has_compatible_config(&self, config: &ContextConfiguration) -> bool {
        let StagingBufferState::Available(buffer) = &self.state else {
            return false;
        };
        buffer.has_compatible_config(config)
    }

    /// Return true if buffer is not mapping or being mapped
    const fn needs_assignment(&self) -> bool {
        matches!(
            self.state,
            StagingBufferState::Unassigned | StagingBufferState::Available(_)
        )
    }

    /// Make buffer available by unmapping / destroying it and then recreating it if needed.
    fn ensure_available(&mut self, config: &ContextConfiguration) -> Result<(), CreateBufferError> {
        let current = match &self.state {
            StagingBufferState::Unassigned => None,
            StagingBufferState::Available(buffer) |
            StagingBufferState::Mapping(buffer) |
            StagingBufferState::Mapped(MappedBuffer { buffer, .. }) => Some(*buffer),
        };
        let recreate = match current {
            None => true,
            Some(buffer) => {
                if buffer.has_compatible_config(config) {
                    // Cancel an in-flight map or release an existing mapping so the
                    // buffer can be reused; it is available afterwards.
                    let _ = self.global.buffer_unmap(self.buffer_id);
                    self.state = StagingBufferState::Available(buffer);
                    false
                } else {
                    self.global.buffer_drop(self.buffer_id);
                    true
                }
            },
        };
        if recreate {
            let buffer_size = config.buffer_size();
            let (_, error) = self.global.device_create_buffer(
                config.device_id,
                &BufferDescriptor {
                    label: None,
                    size: buffer_size,
                    usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                },
                Some(self.buffer_id),
            );
            if let Some(error) = error {
                return Err(error);
            };
            self.state = StagingBufferState::Available(Buffer {
                device_id: config.device_id,
                queue_id: config.queue_id,
                size: buffer_size,
            });
        }
        Ok(())
    }

    /// Makes buffer available and prepares command encoder
    /// that will copy texture to this staging buffer.
    ///
    /// Caller must submit command buffer to queue.
    fn prepare_load_texture_command_buffer(
        &mut self,
        texture_id: TextureId,
        encoder_id: CommandEncoderId,
        command_buffer_id: CommandBufferId,
        config: &ContextConfiguration,
    ) -> Result<CommandBufferId, Box<dyn std::error::Error>> {
        self.ensure_available(config)?;
        let StagingBufferState::Available(buffer) = &self.state else {
            unreachable!("Should be made available by `ensure_available`")
        };
        let device_id = buffer.device_id;
        let command_descriptor = CommandEncoderDescriptor { label: None };
        let (encoder_id, error) = self.global.device_create_command_encoder(
            device_id,
            &command_descriptor,
            Some(encoder_id),
        );
        if let Some(error) = error {
            return Err(error.into());
        };
        let buffer_info = TexelCopyBufferInfo {
            buffer: self.buffer_id,
            layout: TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(config.stride()),
                rows_per_image: None,
            },
        };
        let texture_info = TexelCopyTextureInfo {
            texture: texture_id,
            mip_level: 0,
            origin: Origin3d::ZERO,
            aspect: TextureAspect::All,
        };
        let copy_size = Extent3d {
            width: config.size.width,
            height: config.size.height,
            depth_or_array_layers: 1,
        };
        self.global.command_encoder_copy_texture_to_buffer(
            encoder_id,
            &texture_info,
            &buffer_info,
            &copy_size,
        )?;
        let (command_buffer_id, error) = self.global.command_encoder_finish(
            encoder_id,
            &CommandBufferDescriptor::default(),
            Some(command_buffer_id),
        );
        if let Some((_, error)) = error {
            return Err(error.into());
        };
        Ok(command_buffer_id)
    }

    /// Unmaps the buffer or cancels a mapping operation if one is in progress.
    fn unmap(&mut self) {
        match self.state {
            StagingBufferState::Unassigned | StagingBufferState::Available(_) => {},
            StagingBufferState::Mapping(buffer) |
            StagingBufferState::Mapped(MappedBuffer { buffer, .. }) => {
                let _ = self.global.buffer_unmap(self.buffer_id);
                self.state = StagingBufferState::Available(buffer)
            },
        }
    }

    /// Obtain a snapshot from this buffer if is mapped or return `None` if it is not mapped.
    fn snapshot(&self) -> Option<Snapshot> {
        let StagingBufferState::Mapped(mapped) = &self.state else {
            return None;
        };
        let format = match mapped.image_format {
            ImageFormat::RGBA8 => SnapshotPixelFormat::RGBA,
            ImageFormat::BGRA8 => SnapshotPixelFormat::BGRA,
            _ => unreachable!("GPUCanvasContext does not support other formats per spec"),
        };
        let alpha_mode = if mapped.is_opaque {
            SnapshotAlphaMode::AsOpaque {
                premultiplied: false,
            }
        } else {
            SnapshotAlphaMode::Transparent {
                premultiplied: true,
            }
        };
        let padded_byte_width = mapped.stride();
        let data = mapped.slice();
        let bytes_per_pixel = mapped.image_format.bytes_per_pixel() as usize;
        let mut result_unpadded =
            Vec::<u8>::with_capacity(mapped.image_size.area() as usize * bytes_per_pixel);
        for row in 0..mapped.image_size.height {
            let start = (row * padded_byte_width).try_into().ok()?;
            result_unpadded
                .extend(&data[start..start + mapped.image_size.width as usize * bytes_per_pixel]);
        }
        let mut snapshot =
            Snapshot::from_vec(mapped.image_size, format, alpha_mode, result_unpadded);
        if mapped.is_opaque {
            snapshot.transform(SnapshotAlphaMode::Opaque, snapshot.format())
        }
        Some(snapshot)
    }
}

impl Drop for StagingBuffer {
    fn drop(&mut self) {
        match self.state {
            StagingBufferState::Unassigned => {},
            StagingBufferState::Available(_) |
            StagingBufferState::Mapping(_) |
            StagingBufferState::Mapped(_) => {
                self.global.buffer_drop(self.buffer_id);
            },
        }
    }
}

pub struct WebGpuExternalImages {
    pub image_map: WebGpuExternalImageMap,
    pub locked_ids: FxHashMap<WebGPUContextId, PresentationStagingBuffer>,
}

impl WebGpuExternalImages {
    pub fn new(image_map: WebGpuExternalImageMap) -> Self {
        Self {
            image_map,
            locked_ids: Default::default(),
        }
    }
}

impl WebRenderExternalImageApi for WebGpuExternalImages {
    fn lock(&mut self, id: u64) -> (ExternalImageSource<'_>, Size2D<i32>) {
        let id = WebGPUContextId(id);
        let (presentation, descriptor_size) = {
            let webgpu_contexts = self.image_map.lock().unwrap();
            let Some(context_data) = webgpu_contexts.get(&id) else {
                return (ExternalImageSource::Invalid, Size2D::zero());
            };
            (
                context_data.presentation.clone(),
                context_data.descriptor_size,
            )
        };
        let Some(presentation) = presentation else {
            return (ExternalImageSource::Invalid, Size2D::zero());
        };
        let StagingBufferState::Mapped(mapped_buffer) = &presentation.staging_buffer.state else {
            unreachable!("Presentation staging buffer should be mapped")
        };
        // Descriptor and pixel bytes now travel to WebRender atomically as
        // `SerializableImageData::Raw`, so they cannot skew inside WebRender's
        // queue. Still, guard against a presentation whose size no longer
        // matches the last descriptor sent (e.g. after a resize): WebRender
        // uploads using the descriptor size and ignores the size returned here,
        // which would trip its upload size assert and abort the process.
        // Report the image as invalid instead; WebRender fills it with
        // a dummy value and we lose at most one frame.
        if mapped_buffer.image_size.cast().cast_unit() != descriptor_size {
            warn!(
                "WebGPU canvas descriptor/presentation size mismatch \
                (descriptor: {descriptor_size:?}, presentation: {:?}); \
                dropping frame to avoid texture upload abort",
                mapped_buffer.image_size,
            );
            return (ExternalImageSource::Invalid, Size2D::zero());
        }
        self.locked_ids.insert(id, presentation);
        let presentation = self.locked_ids.get(&id).unwrap();
        let StagingBufferState::Mapped(mapped_buffer) = &presentation.staging_buffer.state else {
            unreachable!("Presentation staging buffer should be mapped")
        };
        let size = mapped_buffer.image_size;
        (
            ExternalImageSource::RawData(mapped_buffer.slice()),
            size.cast().cast_unit(),
        )
    }

    fn unlock(&mut self, id: u64) {
        let id = WebGPUContextId(id);
        let Some(presentation) = self.locked_ids.remove(&id) else {
            return;
        };
        let mut webgpu_contexts = self.image_map.lock().unwrap();
        if let Some(context_data) = webgpu_contexts.get_mut(&id) {
            // We use this to return staging buffer if a newer one exists.
            presentation.maybe_destroy(context_data);
        } else {
            // This will not free this buffer id in script,
            // but that's okay because we still have many free ids.
            drop(presentation);
        }
    }
}

/// Staging buffer currently used for presenting the epoch.
///
/// Users should [`ContextData::replace_presentation`] when done.
#[derive(Clone)]
pub struct PresentationStagingBuffer {
    epoch: Epoch,
    staging_buffer: Arc<StagingBuffer>,
}

impl PresentationStagingBuffer {
    fn new(epoch: Epoch, staging_buffer: StagingBuffer) -> Self {
        Self {
            epoch,
            staging_buffer: Arc::new(staging_buffer),
        }
    }

    /// If the internal staging buffer is not shared,
    /// unmap it and call [`ContextData::return_staging_buffer`] with it.
    fn maybe_destroy(self, context_data: &mut ContextData) {
        if let Some(mut staging_buffer) = Arc::into_inner(self.staging_buffer) {
            staging_buffer.unmap();
            context_data.return_staging_buffer(staging_buffer);
        }
    }
}

/// The embedder process-side representation of what is the `GPUCanvasContext` in script.
pub struct ContextData {
    /// The [`ImageKey`] of the WebRender image associated with this context.
    image_key: Option<ImageKey>,
    /// The current size of this context.
    size: DeviceIntSize,
    /// Size of the image descriptor most recently sent to WebRender for this
    /// context via `add_image`/`update_image`.
    ///
    /// [`WebGpuExternalImages::lock`] compares the presentation against this value
    /// and reports the image as invalid instead of handing out a buffer whose
    /// size no longer matches the descriptor, losing at most one frame.
    descriptor_size: DeviceIntSize,
    /// Staging buffers that are not actively used.
    ///
    /// Staging buffer here are either [`StagingBufferState::Unassigned`] or [`StagingBufferState::Available`].
    /// They are removed from here when they are in process of being mapped or are already mapped.
    inactive_staging_buffers: ArrayVec<StagingBuffer, PRESENTATION_BUFFER_COUNT>,
    /// The [`PresentationStagingBuffer`] of the most recent presentation. This will
    /// be `None` directly after initialization, as clearing is handled completely in
    /// the `ScriptThread`.
    presentation: Option<PresentationStagingBuffer>,
    /// Next epoch to be used
    next_epoch: Epoch,
}

impl ContextData {
    fn new(
        global: &Arc<Global>,
        buffer_ids: ArrayVec<id::BufferId, PRESENTATION_BUFFER_COUNT>,
        size: DeviceIntSize,
    ) -> Self {
        Self {
            image_key: None,
            size,
            descriptor_size: size,
            inactive_staging_buffers: buffer_ids
                .iter()
                .map(|buffer_id| StagingBuffer::new(global.clone(), *buffer_id))
                .collect(),
            presentation: None,
            next_epoch: Epoch(1),
        }
    }

    /// Returns `None` if no staging buffer is unused or failure when making it available
    fn get_or_make_available_buffer(
        &'_ mut self,
        config: &ContextConfiguration,
    ) -> Option<StagingBuffer> {
        self.inactive_staging_buffers
            .iter()
            // Try to get first preallocated GPUBuffer.
            .position(|staging_buffer| {
                staging_buffer.is_available_and_has_compatible_config(config)
            })
            // Fall back to the first inactive staging buffer.
            .or_else(|| {
                self.inactive_staging_buffers
                    .iter()
                    .position(|staging_buffer| staging_buffer.needs_assignment())
            })
            // Or just the use first one.
            .or_else(|| {
                if self.inactive_staging_buffers.is_empty() {
                    None
                } else {
                    Some(0)
                }
            })
            .and_then(|index| {
                let mut staging_buffer = self.inactive_staging_buffers.remove(index);
                if staging_buffer.ensure_available(config).is_ok() {
                    Some(staging_buffer)
                } else {
                    // If we fail to make it available, return it to the list of inactive staging buffers.
                    self.inactive_staging_buffers.push(staging_buffer);
                    None
                }
            })
    }

    /// Destroy the context that this [`ContextData`] represents,
    /// freeing all of its buffers, and deleting the associated WebRender image.
    fn destroy(
        mut self,
        script_sender: &GenericSender<WebGPUMsg>,
        paint_api: &CrossProcessPaintApi,
    ) {
        // This frees the id in the `ScriptThread`.
        for staging_buffer in self.inactive_staging_buffers {
            if let Err(error) = script_sender.send(WebGPUMsg::FreeBuffer(staging_buffer.buffer_id))
            {
                warn!(
                    "Unable to send FreeBuffer({:?}) ({error})",
                    staging_buffer.buffer_id
                );
            };
        }
        if let Some(image_key) = self.image_key.take() {
            paint_api.delete_image(image_key);
        }
    }

    /// Advance the [`Epoch`] and return the new one.
    fn next_epoch(&mut self) -> Epoch {
        let epoch = self.next_epoch;
        self.next_epoch.next();
        epoch
    }

    /// If the given [`PresentationStagingBuffer`] is for a newer presentation, replace the existing
    /// one and return true. Deallocate the older one by calling [`Self::return_staging_buffer`] on it.
    /// Returns false and deallocates the given buffer when it carries an older epoch.
    fn replace_presentation(&mut self, presentation: PresentationStagingBuffer) -> bool {
        let adopted = presentation.epoch >=
            self.presentation
                .as_ref()
                .map(|p| p.epoch)
                .unwrap_or_default();
        let stale_presentation = if adopted {
            self.presentation.replace(presentation)
        } else {
            Some(presentation)
        };
        if let Some(stale_presentation) = stale_presentation {
            stale_presentation.maybe_destroy(self);
        }
        adopted
    }

    fn clear_presentation(&mut self) {
        if let Some(stale_presentation) = self.presentation.take() {
            stale_presentation.maybe_destroy(self);
        }
    }

    fn return_staging_buffer(&mut self, staging_buffer: StagingBuffer) {
        self.inactive_staging_buffers.push(staging_buffer)
    }
}

impl crate::WGPU {
    pub(crate) fn create_context(
        &self,
        context_id: WebGPUContextId,
        size: DeviceIntSize,
        buffer_ids: ArrayVec<id::BufferId, PRESENTATION_BUFFER_COUNT>,
    ) {
        let context_data = ContextData::new(&self.global, buffer_ids, size);
        assert!(
            self.wgpu_image_map
                .lock()
                .unwrap()
                .insert(context_id, context_data)
                .is_none(),
            "Context should be created only once!"
        );
    }

    pub(crate) fn set_image_key(&self, context_id: WebGPUContextId, image_key: ImageKey) {
        let mut webgpu_contexts = self.wgpu_image_map.lock().unwrap();
        let context_data = webgpu_contexts.get_mut(&context_id).unwrap();

        if let Some(old_image_key) = context_data.image_key.replace(image_key) {
            self.paint_api.delete_image(old_image_key);
        }

        // Send the currently adopted presentation (or a cleared image) as raw
        // bytes, so the descriptor and the pixels reach WebRender atomically.
        let (descriptor, data) = current_descriptor_and_data(context_data);
        context_data.descriptor_size = descriptor.size;
        self.paint_api
            .add_image(image_key, descriptor, data, false);
    }

    pub(crate) fn get_image(
        &self,
        context_id: WebGPUContextId,
        pending_texture: Option<PendingTexture>,
        sender: GenericSender<SharedSnapshot>,
    ) {
        let mut webgpu_contexts = self.wgpu_image_map.lock().unwrap();
        let context_data = webgpu_contexts.get_mut(&context_id).unwrap();
        if let Some(PendingTexture {
            texture_id,
            encoder_id,
            command_buffer_id,
            configuration,
        }) = pending_texture
        {
            let Some(staging_buffer) = context_data.get_or_make_available_buffer(&configuration)
            else {
                warn!("Failure obtaining available staging buffer");
                sender
                    .send(SharedSnapshot::cleared(configuration.size))
                    .unwrap();
                return;
            };

            let epoch = context_data.next_epoch();
            let wgpu_image_map = self.wgpu_image_map.clone();
            let paint_api = self.paint_api.clone();
            let sender = sender;
            drop(webgpu_contexts);
            self.texture_download(
                texture_id,
                encoder_id,
                command_buffer_id,
                staging_buffer,
                configuration,
                move |staging_buffer| {
                    let mut webgpu_contexts = wgpu_image_map.lock().unwrap();
                    let context_data = webgpu_contexts.get_mut(&context_id).unwrap();
                    sender
                        .send(
                            staging_buffer
                                .snapshot()
                                .as_ref()
                                .map(Snapshot::to_shared)
                                .unwrap_or_else(|| SharedSnapshot::cleared(configuration.size)),
                        )
                        .unwrap();
                    if staging_buffer.is_mapped() {
                        let shared = {
                            let StagingBufferState::Mapped(mapped) = &staging_buffer.state
                            else {
                                unreachable!("checked by `is_mapped`")
                            };
                            GenericSharedMemory::from_bytes(mapped.slice())
                        };
                        let adopted = context_data.replace_presentation(
                            PresentationStagingBuffer::new(epoch, staging_buffer),
                        );
                        // Keep WebRender's copy in sync with the newly adopted
                        // presentation; `present` may never run for this frame.
                        if adopted {
                            if let Some(image_key) = context_data.image_key {
                                let descriptor: ImageDescriptor = configuration.into();
                                context_data.descriptor_size = descriptor.size;
                                paint_api.update_image(
                                    image_key,
                                    descriptor,
                                    SerializableImageData::Raw(shared),
                                    None,
                                );
                            }
                        }
                    } else {
                        // failure
                        context_data.return_staging_buffer(staging_buffer);
                    }
                },
            );
        } else {
            sender
                .send(
                    context_data
                        .presentation
                        .as_ref()
                        .and_then(|presentation_staging_buffer| {
                            presentation_staging_buffer.staging_buffer.snapshot()
                        })
                        .unwrap_or_else(Snapshot::empty)
                        .to_shared(),
                )
                .unwrap();
        }
    }

    /// Read the texture to the staging buffer, map it to CPU memory, and update the
    /// image in WebRender when complete.
    pub(crate) fn present(
        &self,
        context_id: WebGPUContextId,
        pending_texture: Option<PendingTexture>,
        size: Size2D<u32>,
        canvas_epoch: Epoch,
    ) {
        let mut webgpu_contexts = self.wgpu_image_map.lock().unwrap();
        let context_data = webgpu_contexts.get_mut(&context_id).unwrap();

        let Some(image_key) = context_data.image_key else {
            return;
        };

        // Keep the recorded canvas size current; `set_image_key` may have
        // stored an older size.
        context_data.size = size.cast_unit().cast();

        let Some(PendingTexture {
            texture_id,
            encoder_id,
            command_buffer_id,
            configuration,
        }) = pending_texture
        else {
            context_data.clear_presentation();
            let (descriptor, data) = cleared_descriptor_and_data(context_data.size);
            context_data.descriptor_size = descriptor.size;
            self.paint_api.update_image(
                image_key,
                descriptor,
                data,
                Some(canvas_epoch),
            );
            return;
        };
        let Some(staging_buffer) = context_data.get_or_make_available_buffer(&configuration) else {
            warn!("Failure obtaining available staging buffer");
            context_data.clear_presentation();
            let (descriptor, data) =
                cleared_descriptor_and_data(configuration.size.cast().cast_unit());
            context_data.descriptor_size = descriptor.size;
            self.paint_api.update_image(
                image_key,
                descriptor,
                data,
                Some(canvas_epoch),
            );
            return;
        };
        let epoch = context_data.next_epoch();
        let wgpu_image_map = self.wgpu_image_map.clone();
        let paint_api = self.paint_api.clone();
        drop(webgpu_contexts);
        self.texture_download(
            texture_id,
            encoder_id,
            command_buffer_id,
            staging_buffer,
            configuration,
            move |staging_buffer| {
                let mut webgpu_contexts = wgpu_image_map.lock().unwrap();
                let context_data = webgpu_contexts.get_mut(&context_id).unwrap();
                if staging_buffer.is_mapped() {
                    // Copy the pixels out before the staging buffer moves into
                    // the presentation slot.
                    let shared = {
                        let StagingBufferState::Mapped(mapped) = &staging_buffer.state
                        else {
                            unreachable!("checked by `is_mapped`")
                        };
                        GenericSharedMemory::from_bytes(mapped.slice())
                    };
                    let adopted = context_data.replace_presentation(
                        PresentationStagingBuffer::new(epoch, staging_buffer),
                    );
                    if adopted {
                        // Fresh frame: descriptor and pixels travel atomically.
                        let descriptor: ImageDescriptor = configuration.into();
                        context_data.descriptor_size = descriptor.size;
                        paint_api.update_image(
                            image_key,
                            descriptor,
                            SerializableImageData::Raw(shared),
                            Some(canvas_epoch),
                        );
                    } else {
                        // Stale frame: never overwrite WebRender with the old
                        // size; resend the currently adopted presentation (or a
                        // cleared image) so the FrameDelayer still observes
                        // this epoch.
                        let (descriptor, data) = current_descriptor_and_data(context_data);
                        context_data.descriptor_size = descriptor.size;
                        paint_api.update_image(
                            image_key,
                            descriptor,
                            data,
                            Some(canvas_epoch),
                        );
                    }
                } else {
                    context_data.return_staging_buffer(staging_buffer);
                    let current_epoch = context_data
                        .presentation
                        .as_ref()
                        .map(|presentation| presentation.epoch)
                        .unwrap_or_default();
                    if epoch >= current_epoch {
                        context_data.clear_presentation();
                        let (descriptor, data) =
                            cleared_descriptor_and_data(context_data.size);
                        context_data.descriptor_size = descriptor.size;
                        paint_api.update_image(
                            image_key,
                            descriptor,
                            data,
                            Some(canvas_epoch),
                        );
                    } else {
                        // Stale failure: keep the adopted presentation, but the
                        // FrameDelayer still needs an `update_image` carrying
                        // this epoch.
                        let (descriptor, data) = current_descriptor_and_data(context_data);
                        context_data.descriptor_size = descriptor.size;
                        paint_api.update_image(
                            image_key,
                            descriptor,
                            data,
                            Some(canvas_epoch),
                        );
                    }
                }
            },
        );
    }

    /// Copies data from provided texture using `encoder_id` to the provided [`StagingBuffer`].
    ///
    /// `callback` is guaranteed to be called.
    ///
    /// Returns a [`StagingBuffer`] with the [`StagingBufferState::Mapped`] state
    /// on success or [`StagingBufferState::Available`] on failure.
    fn texture_download(
        &self,
        texture_id: TextureId,
        encoder_id: CommandEncoderId,
        command_buffer_id: CommandBufferId,
        mut staging_buffer: StagingBuffer,
        config: ContextConfiguration,
        callback: impl FnOnce(StagingBuffer) + Send + 'static,
    ) {
        let Ok(command_buffer_id) = staging_buffer.prepare_load_texture_command_buffer(
            texture_id,
            encoder_id,
            command_buffer_id,
            &config,
        ) else {
            return callback(staging_buffer);
        };
        let StagingBufferState::Available(buffer) = &staging_buffer.state else {
            unreachable!("`prepare_load_texture_command_buffer` should make buffer available")
        };
        let buffer_id = staging_buffer.buffer_id;
        let buffer_size = buffer.size;
        {
            let _guard = self.poller.lock();
            let result = self
                .global
                .queue_submit(buffer.queue_id, &[command_buffer_id]);
            if result.is_err() {
                return callback(staging_buffer);
            }
        }
        staging_buffer.state = match staging_buffer.state {
            StagingBufferState::Available(buffer) => StagingBufferState::Mapping(buffer),
            _ => unreachable!("`prepare_load_texture_command_buffer` should make buffer available"),
        };
        let map_callback = {
            let token = self.poller.token();
            Box::new(move |result: Result<(), BufferAccessError>| {
                drop(token);
                staging_buffer.state = match staging_buffer.state {
                    StagingBufferState::Mapping(buffer) => {
                        if let Ok((data, len)) = result.and_then(|_| {
                            staging_buffer.global.buffer_get_mapped_range(
                                staging_buffer.buffer_id,
                                0,
                                Some(buffer.size),
                            )
                        }) {
                            StagingBufferState::Mapped(MappedBuffer {
                                buffer,
                                data,
                                len,
                                image_size: config.size,
                                image_format: config.format,
                                is_opaque: config.is_opaque,
                            })
                        } else {
                            StagingBufferState::Available(buffer)
                        }
                    },
                    _ => {
                        unreachable!("Mapping buffer should have StagingBufferState::Mapping state")
                    },
                };
                callback(staging_buffer);
            })
        };
        let map_op = BufferMapOperation {
            host: HostMap::Read,
            callback: Some(map_callback),
        };
        // error is handled by map_callback
        let _ = self
            .global
            .buffer_map_async(buffer_id, 0, Some(buffer_size), map_op);
        self.poller.wake();
    }

    pub(crate) fn destroy_context(&mut self, context_id: WebGPUContextId) {
        self.wgpu_image_map
            .lock()
            .unwrap()
            .remove(&context_id)
            .unwrap()
            .destroy(&self.script_sender, &self.paint_api);
    }
}
