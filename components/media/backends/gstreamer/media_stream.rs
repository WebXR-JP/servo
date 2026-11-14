/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::any::Any;
use std::sync::{Arc, LazyLock, Mutex};

use glib::BoolError;
use gstreamer;
use gstreamer::prelude::*;
use log::{info, warn};
use servo_media_streams::registry::{
    MediaStreamId, get_stream, register_stream, stop_stream, unregister_stream,
};
use servo_media_streams::{
    MediaOutput, MediaSocket, MediaStream, MediaStreamSettings, MediaStreamType,
};

use super::BACKEND_BASE_TIME;

pub static RTP_CAPS_OPUS: LazyLock<gstreamer::Caps> = LazyLock::new(|| {
    gstreamer::Caps::builder("application/x-rtp")
        .field("media", "audio")
        .field("encoding-name", "OPUS")
        .build()
});

pub static RTP_CAPS_VP8: LazyLock<gstreamer::Caps> = LazyLock::new(|| {
    gstreamer::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", "VP8")
        .build()
});

pub struct GStreamerMediaStream {
    id: Option<MediaStreamId>,
    type_: MediaStreamType,
    elements: Vec<gstreamer::Element>,
    pipeline: Option<gstreamer::Pipeline>,
    branch: Option<CaptureBranch>,
    /// 再生用に共有する encoder 出力の tee と、player 専用 branch の ID。
    /// 元 branch が WebRTC 送出などで既に link 済みの場合は clone を使う。
    playback_output: Option<(gstreamer::Element, gstreamer::Pipeline)>,
    playback_clone: Option<MediaStreamId>,
}

/// 1つの入力を複数のトラック枝へ分ける。入力は最後の枝が停止するまで動かす。
struct SharedSource {
    pipeline: gstreamer::Pipeline,
    tee: gstreamer::Element,
    branches: usize,
    closed: bool,
    /// 選択したデバイスの公開 ID。合成入力では `None`。
    device_id: Option<String>,
    /// 最後の枝が止まったとき、または共有入力の破棄時に一度だけ実行する後始末。
    /// 画面共有では portal の `Session.Close` を入れる。
    cleanup: Option<Box<dyn FnOnce() + Send>>,
}

struct CaptureBranch {
    source: Arc<Mutex<SharedSource>>,
    queue: gstreamer::Element,
    sink: gstreamer::Element,
    tee_pad: gstreamer::Pad,
    gate: Arc<TrackGate>,
    controls: Arc<Mutex<TrackControls>>,
    controls_owner: bool,
    linked: bool,
    tee_pad_released: bool,
    queue_added: bool,
    sink_added: bool,
    counted: bool,
    stopped: bool,
}

struct TrackGate {
    element: gstreamer::Element,
    type_: MediaStreamType,
}

struct TrackControls {
    enabled: bool,
    ended: bool,
    gates: Vec<std::sync::Weak<TrackGate>>,
}

fn set_gate_enabled(gate: &TrackGate, enabled: bool) {
    match gate.type_ {
        MediaStreamType::Audio => gate
            .element
            .set_property("volume", if enabled { 1.0 } else { 0.0 }),
        MediaStreamType::Video => gate.element.set_property("drop", !enabled),
    }
}

fn set_track_controls_enabled(controls: &mut TrackControls, enabled: bool) {
    controls.enabled = enabled;
    let enabled = controls.enabled && !controls.ended;
    controls.gates.retain(|gate| {
        if let Some(gate) = gate.upgrade() {
            set_gate_enabled(&gate, enabled);
            true
        } else {
            false
        }
    });
}

/// pipeline の状態遷移失敗時に、bus に届いている Error メッセージを短時間だけ拾う。
/// 失敗理由を `NotReadable` の文字列へ含めるために使う。
fn take_bus_error(pipeline: &gstreamer::Pipeline) -> Option<String> {
    let bus = pipeline.bus()?;
    let message = bus.timed_pop_filtered(
        gstreamer::ClockTime::from_mseconds(200),
        &[gstreamer::MessageType::Error],
    )?;
    match message.view() {
        gstreamer::MessageView::Error(error) => Some(match error.debug() {
            Some(debug) => format!("{} (debug: {debug})", error.error()),
            None => error.error().to_string(),
        }),
        _ => None,
    }
}

fn remove_unregistered_branch(
    source: &mut SharedSource,
    queue: &gstreamer::Element,
    sink: &gstreamer::Element,
    tee_pad: Option<&gstreamer::Pad>,
    linked: bool,
) -> Vec<String> {
    let mut errors = Vec::new();
    if linked {
        if let (Some(tee_pad), Some(queue_pad)) = (tee_pad, queue.static_pad("sink")) {
            if let Err(error) = tee_pad.unlink(&queue_pad) {
                errors.push(error.to_string());
            }
        }
    }
    if let Some(tee_pad) = tee_pad {
        source.tee.release_request_pad(tee_pad);
    }
    for element in [sink, queue] {
        if let Err(error) = element.set_state(gstreamer::State::Null) {
            errors.push(error.to_string());
        }
    }
    for element in [queue, sink] {
        if element.parent().is_some() {
            if let Err(error) = source.pipeline.remove(element) {
                errors.push(error.to_string());
            }
        }
    }
    errors
}

impl CaptureBranch {
    fn stop(&mut self) -> Result<(), String> {
        if self.stopped {
            return Ok(());
        }
        // 途中の停止エラーでも二重実行しないよう、先に確定する。
        self.stopped = true;

        if self.controls_owner {
            let mut controls = self.controls.lock().unwrap();
            controls.ended = true;
            set_track_controls_enabled(&mut controls, false);
        } else {
            set_gate_enabled(&self.gate, false);
        }

        // 途中で早期 return せず、すべての後始末を試す。最後の枝なら
        // pipeline の停止成否に関わらず cleanup（portal の Session.Close）へ進む。
        let mut errors: Vec<String> = Vec::new();
        let mut source = self.source.lock().unwrap();
        if self.linked {
            match self.queue.static_pad("sink") {
                Some(queue_pad) => {
                    if let Err(error) = self.tee_pad.unlink(&queue_pad) {
                        errors.push(error.to_string());
                    }
                },
                None => errors.push(String::from("queue の入力 pad が無い")),
            }
            self.linked = false;
        }
        if !self.tee_pad_released {
            source.tee.release_request_pad(&self.tee_pad);
            self.tee_pad_released = true;
        }
        if let Err(error) = self.queue.set_state(gstreamer::State::Null) {
            errors.push(error.to_string());
        }
        if let Err(error) = self.sink.set_state(gstreamer::State::Null) {
            errors.push(error.to_string());
        }
        if self.queue_added {
            if let Err(error) = source.pipeline.remove(&self.queue) {
                errors.push(error.to_string());
            }
            self.queue_added = false;
        }
        if self.sink_added {
            if let Err(error) = source.pipeline.remove(&self.sink) {
                errors.push(error.to_string());
            }
            self.sink_added = false;
        }
        if self.counted {
            match source.branches.checked_sub(1) {
                Some(branches) => source.branches = branches,
                None => errors.push(String::from("共有メディア入力の枝数が不正")),
            }
            self.counted = false;
        }
        if source.branches == 0 {
            source.closed = true;
            if let Err(error) = source.pipeline.set_state(gstreamer::State::Null) {
                errors.push(error.to_string());
            }
            let cleanup = source.cleanup.take();
            drop(source);
            // ロックを離してから実行する。portal の Close は別スレッドへ投げる契約。
            if let Some(cleanup) = cleanup {
                cleanup();
            }
        } else {
            drop(source);
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

impl Drop for SharedSource {
    fn drop(&mut self) {
        // 最後の枝の停止経路を通らずに破棄された場合も、pipeline を止めてから
        // cleanup を一度だけ実行する。
        let _ = self.pipeline.set_state(gstreamer::State::Null);
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

impl MediaStream for GStreamerMediaStream {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_mut_any(&mut self) -> &mut dyn Any {
        self
    }

    fn set_id(&mut self, id: MediaStreamId) {
        self.id = Some(id);
    }

    fn ty(&self) -> MediaStreamType {
        self.type_
    }

    fn set_enabled(&mut self, enabled: bool) -> Result<(), String> {
        let branch = self
            .branch
            .as_ref()
            .ok_or_else(|| String::from("独立したトラック枝ではない"))?;
        if branch.stopped {
            return Err(String::from("終了したトラック枝は変更できない"));
        }
        if !branch.controls_owner {
            return Err(String::from("共有制御枝の有効状態は元トラックから変更する"));
        }
        set_track_controls_enabled(&mut branch.controls.lock().unwrap(), enabled);
        Ok(())
    }

    fn clone_stream(&mut self) -> Result<MediaStreamId, String> {
        let branch = self
            .branch
            .as_ref()
            .ok_or_else(|| String::from("独立したトラック枝ではない"))?;
        if branch.stopped {
            return Err(String::from("終了したトラック枝は複製できない"));
        }
        Self::create_branch(branch.source.clone(), self.type_)
    }

    fn stop(&mut self) -> Result<(), String> {
        // 停止順は元 branch → player 用 clone。最後の branch 停止で portal
        // cleanup が走る。
        let result = match self.branch.as_mut() {
            Some(branch) => branch.stop(),
            None => Err(String::from("独立したトラック枝ではない")),
        };
        if let Some(clone) = self.playback_clone.take() {
            if let Err(error) = stop_stream(&clone) {
                warn!("再生用 branch clone を止められない: {error}");
            }
            unregister_stream(&clone);
            self.playback_output = None;
        }
        result
    }

    fn settings(&self) -> MediaStreamSettings {
        // caps が未確定でも device_id は返す。早期 return で捨てない。
        let device_id = self
            .branch
            .as_ref()
            .and_then(|branch| branch.source.lock().unwrap().device_id.clone());
        let mut settings = MediaStreamSettings {
            device_id,
            ..Default::default()
        };
        let Some(branch) = self.branch.as_ref() else {
            return settings;
        };
        let source = branch.source.lock().unwrap();
        let Some(pad) = source.tee.static_pad("sink") else {
            return settings;
        };
        let Some(caps) = pad.current_caps() else {
            return settings;
        };
        if !caps.is_fixed() || caps.size() != 1 {
            return settings;
        }
        let Some(caps) = caps.structure(0) else {
            return settings;
        };
        settings.width = caps
            .get::<i32>("width")
            .ok()
            .and_then(|width| u32::try_from(width).ok());
        settings.height = caps
            .get::<i32>("height")
            .ok()
            .and_then(|height| u32::try_from(height).ok());
        settings.aspect_ratio = settings
            .width
            .zip(settings.height)
            .and_then(|(width, height)| (height != 0).then_some(width as f64 / height as f64));
        settings.frame_rate = caps
            .get::<gstreamer::Fraction>("framerate")
            .ok()
            .and_then(|rate| {
                (rate.denom() != 0).then_some(rate.numer() as f64 / rate.denom() as f64)
            });
        settings.sample_rate = caps
            .get::<i32>("rate")
            .ok()
            .and_then(|rate| u32::try_from(rate).ok());
        settings
    }
}

impl GStreamerMediaStream {
    /// Registry ID assigned by `register_stream`. Used to make player stream
    /// attachment idempotent.
    pub(crate) fn id(&self) -> Option<MediaStreamId> {
        self.id
    }

    pub fn new(type_: MediaStreamType, elements: Vec<gstreamer::Element>) -> Self {
        Self {
            id: None,
            type_,
            elements,
            pipeline: None,
            branch: None,
            playback_output: None,
            playback_clone: None,
        }
    }

    fn from_branch(
        type_: MediaStreamType,
        source: Arc<Mutex<SharedSource>>,
        queue: gstreamer::Element,
        sink: gstreamer::Element,
        tee_pad: gstreamer::Pad,
        shared_controls: Option<Arc<Mutex<TrackControls>>>,
    ) -> Result<Self, String> {
        let proxy = gstreamer::ElementFactory::make("proxysrc")
            .property("proxysink", &sink)
            .build()
            .map_err(|error| error.to_string())?;
        let gate = match type_ {
            MediaStreamType::Audio => gstreamer::ElementFactory::make("volume")
                .property("volume", 1.0f64)
                .build(),
            MediaStreamType::Video => gstreamer::ElementFactory::make("valve")
                .property("drop", false)
                .build(),
        }
        .map_err(|error| error.to_string())?;
        let mut stream = match type_ {
            MediaStreamType::Audio => Self::new(
                type_,
                vec![
                    proxy,
                    gate.clone(),
                    gstreamer::ElementFactory::make("queue")
                        .build()
                        .map_err(|error| error.to_string())?,
                    gstreamer::ElementFactory::make("audioconvert")
                        .build()
                        .map_err(|error| error.to_string())?,
                    gstreamer::ElementFactory::make("audioresample")
                        .build()
                        .map_err(|error| error.to_string())?,
                    gstreamer::ElementFactory::make("queue")
                        .build()
                        .map_err(|error| error.to_string())?,
                ],
            ),
            MediaStreamType::Video => Self::new(
                type_,
                vec![
                    proxy,
                    gate.clone(),
                    gstreamer::ElementFactory::make("queue")
                        .build()
                        .map_err(|error| error.to_string())?,
                    gstreamer::ElementFactory::make("videoconvert")
                        .build()
                        .map_err(|error| error.to_string())?,
                    gstreamer::ElementFactory::make("queue")
                        .build()
                        .map_err(|error| error.to_string())?,
                ],
            ),
        };
        let gate = Arc::new(TrackGate {
            element: gate,
            type_,
        });
        let (controls, controls_owner) = match shared_controls {
            Some(controls) => (controls, false),
            None => (
                Arc::new(Mutex::new(TrackControls {
                    enabled: true,
                    ended: false,
                    gates: Vec::new(),
                })),
                true,
            ),
        };
        let enabled = {
            let mut controls = controls.lock().unwrap();
            controls.gates.retain(|gate| gate.strong_count() != 0);
            controls.gates.push(Arc::downgrade(&gate));
            controls.enabled && !controls.ended
        };
        set_gate_enabled(&gate, enabled);
        stream.branch = Some(CaptureBranch {
            source,
            queue,
            sink,
            tee_pad,
            gate,
            controls,
            controls_owner,
            linked: true,
            tee_pad_released: false,
            queue_added: true,
            sink_added: true,
            counted: true,
            stopped: false,
        });
        Ok(stream)
    }

    fn create_shared_with_device(
        source: gstreamer::Element,
        device_id: Option<String>,
    ) -> Result<Arc<Mutex<SharedSource>>, String> {
        Self::create_shared_with_device_and_cleanup(source, device_id, None)
    }

    fn create_shared_with_device_and_cleanup(
        source: gstreamer::Element,
        device_id: Option<String>,
        cleanup: Option<Box<dyn FnOnce() + Send>>,
    ) -> Result<Arc<Mutex<SharedSource>>, String> {
        let pipeline = gstreamer::Pipeline::with_name("gstreamer media track source");
        pipeline.set_start_time(gstreamer::ClockTime::NONE);
        pipeline.set_base_time(*BACKEND_BASE_TIME);
        pipeline.use_clock(Some(&gstreamer::SystemClock::obtain()));
        let tee = gstreamer::ElementFactory::make("tee")
            // 最初の出力枝を接続する前でも入力をエラーにせず、getUserMediaの生成順序を許容する。
            .property("allow-not-linked", true)
            .build()
            .map_err(|error| error.to_string())?;
        if source
            .factory()
            .is_some_and(|factory| factory.name() == "pipewiresrc")
        {
            let target_object = source.property::<Option<String>>("target-object");
            let path = source.property::<Option<String>>("path");
            info!(
                "create_shared_with_device: source={} target-object={target_object:?} \
                 path={path:?}",
                source.name()
            );
        }
        let setup_result = (|| -> Result<(), String> {
            pipeline
                .add_many([&source, &tee])
                .map_err(|error| error.to_string())?;
            source.link(&tee).map_err(|error| error.to_string())?;
            if let Err(error) = pipeline.set_state(gstreamer::State::Playing) {
                return Err(match take_bus_error(&pipeline) {
                    Some(detail) => format!("{error}; GStreamer: {detail}"),
                    None => error.to_string(),
                });
            }
            Ok(())
        })();
        if let Err(error) = setup_result {
            // pipeline を止めてから cleanup する。portal のセッションを残さない。
            let null_result = pipeline.set_state(gstreamer::State::Null);
            if let Some(cleanup) = cleanup {
                cleanup();
            }
            return Err(match null_result {
                Ok(_) => error,
                Err(cleanup_error) => {
                    format!("{error}; 入力パイプラインの停止にも失敗: {cleanup_error}")
                },
            });
        }
        Ok(Arc::new(Mutex::new(SharedSource {
            pipeline,
            tee,
            branches: 0,
            closed: false,
            device_id,
            cleanup,
        })))
    }

    fn create_branch(
        source: Arc<Mutex<SharedSource>>,
        type_: MediaStreamType,
    ) -> Result<MediaStreamId, String> {
        Self::create_branch_with_controls(source, type_, None)
    }

    fn create_branch_with_controls(
        source: Arc<Mutex<SharedSource>>,
        type_: MediaStreamType,
        controls: Option<Arc<Mutex<TrackControls>>>,
    ) -> Result<MediaStreamId, String> {
        let mut source_guard = source.lock().unwrap();
        if source_guard.closed {
            return Err(String::from("共有メディア入力は既に終了している"));
        }
        let queue = gstreamer::ElementFactory::make("queue")
            // 枝の受信側が停止しても、ライブ入力全体を詰まらせず古い音声を捨てる。
            .property_from_str("leaky", "downstream")
            .property("max-size-buffers", 0u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 250_000_000u64)
            .build()
            .map_err(|error| error.to_string())?;
        let sink = gstreamer::ElementFactory::make("proxysink")
            .build()
            .map_err(|error| error.to_string())?;
        let mut tee_pad = None;
        let mut linked = false;
        let setup_result = (|| -> Result<(), String> {
            source_guard
                .pipeline
                .add_many([&queue, &sink])
                .map_err(|error| error.to_string())?;
            queue.link(&sink).map_err(|error| error.to_string())?;
            // 下流から先に親状態へ揃える。tee と繋ぐ前に受入可能にしておくことで、
            // ライブ入力がまだ NULL の pad へ送出して送出タスクを止めるのを防ぐ。
            // tee 側の接続は活性 pad 同士で行うため、その後の送出は直ちに流れる。
            sink.sync_state_with_parent()
                .map_err(|error| error.to_string())?;
            queue
                .sync_state_with_parent()
                .map_err(|error| error.to_string())?;
            let requested_pad = source_guard
                .tee
                .request_pad_simple("src_%u")
                .ok_or_else(|| String::from("tee の出力 pad を作れない"))?;
            tee_pad = Some(requested_pad.clone());
            let queue_pad = queue
                .static_pad("sink")
                .ok_or_else(|| String::from("queue の入力 pad が無い"))?;
            requested_pad
                .link(&queue_pad)
                .map_err(|error| error.to_string())?;
            linked = true;
            Ok(())
        })();
        if let Err(error) = setup_result {
            let cleanup_errors = remove_unregistered_branch(
                &mut source_guard,
                &queue,
                &sink,
                tee_pad.as_ref(),
                linked,
            );
            return Err(if cleanup_errors.is_empty() {
                error
            } else {
                format!("{error}; 枝の後始末に失敗: {}", cleanup_errors.join("; "))
            });
        }

        let tee_pad = tee_pad.ok_or_else(|| String::from("tee の出力 pad が無い"))?;
        let stream = match Self::from_branch(
            type_,
            source.clone(),
            queue.clone(),
            sink.clone(),
            tee_pad.clone(),
            controls,
        ) {
            Ok(stream) => stream,
            Err(error) => {
                let cleanup_errors = remove_unregistered_branch(
                    &mut source_guard,
                    &queue,
                    &sink,
                    Some(&tee_pad),
                    true,
                );
                return Err(if cleanup_errors.is_empty() {
                    error
                } else {
                    format!("{error}; 枝の後始末に失敗: {}", cleanup_errors.join("; "))
                });
            },
        };
        source_guard.branches += 1;
        drop(source_guard);
        Ok(register_stream(Arc::new(Mutex::new(stream))))
    }

    fn create_shared_from(
        source: gstreamer::Element,
        type_: MediaStreamType,
    ) -> Result<MediaStreamId, String> {
        Self::create_shared_from_with_device(source, type_, None)
    }

    fn create_shared_from_with_device(
        source: gstreamer::Element,
        type_: MediaStreamType,
        device_id: Option<String>,
    ) -> Result<MediaStreamId, String> {
        Self::create_shared_from_with_cleanup(source, type_, device_id, None)
    }

    fn create_shared_from_with_cleanup(
        source: gstreamer::Element,
        type_: MediaStreamType,
        device_id: Option<String>,
        cleanup: Option<Box<dyn FnOnce() + Send>>,
    ) -> Result<MediaStreamId, String> {
        let shared = Self::create_shared_with_device_and_cleanup(source, device_id, cleanup)?;
        match Self::create_branch(shared.clone(), type_) {
            Ok(id) => Ok(id),
            Err(error) => {
                let mut source = shared.lock().unwrap();
                source.closed = true;
                let pipeline_result = source.pipeline.set_state(gstreamer::State::Null);
                let cleanup = source.cleanup.take();
                drop(source);
                // 枝の生成に失敗した時点でストリームは登録されない。portal の
                // セッションをここで確実に閉じる。
                if let Some(cleanup) = cleanup {
                    cleanup();
                }
                Err(match pipeline_result {
                    Ok(_) => error,
                    Err(cleanup_error) => {
                        format!("{error}; 入力パイプラインの停止にも失敗: {cleanup_error}")
                    },
                })
            },
        }
    }

    pub fn caps(&self) -> &gstreamer::Caps {
        match self.type_ {
            MediaStreamType::Audio => &RTP_CAPS_OPUS,
            MediaStreamType::Video => &RTP_CAPS_VP8,
        }
    }

    pub fn caps_with_payload(&self, payload: i32) -> gstreamer::Caps {
        match self.type_ {
            MediaStreamType::Audio => gstreamer::Caps::builder("application/x-rtp")
                .field("media", "audio")
                .field("encoding-name", "OPUS")
                .field("payload", payload)
                .build(),
            MediaStreamType::Video => gstreamer::Caps::builder("application/x-rtp")
                .field("media", "video")
                .field("encoding-name", "VP8")
                .field("payload", payload)
                .build(),
        }
    }

    pub fn src_element(&self) -> gstreamer::Element {
        self.elements.last().unwrap().clone()
    }

    /// 枝の全要素を親パイプラインの状態へ同期する。
    ///
    /// 親が PLAYING の場合、枝全体を PLAYING に揃える。attach 直後でも別経路で
    /// 枝を作った場合でも、link 前に送出側が止まっていないことを保証するために使う。
    /// 要素を Null に落とさないため、送出中のタスクを壊さない。
    /// 止まった送出タスクの起こし直しには使えない（同状態への同期は無操作のため）。
    /// 起こし直しは [`Self::restart_branch_flow`] を使う。
    pub fn sync_branch_with_parent(&self) -> Result<(), String> {
        if self.pipeline.is_none() {
            return Err(String::from("パイプラインに接続していない枝は同期できない"));
        }
        for element in &self.elements {
            element
                .sync_state_with_parent()
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// 止まった枝全体の送出を起こし直す。
    ///
    /// 上流要素がまだ NULL/READY の下流へ送出すると送出タスクが止まり、その後の
    /// 同状態への同期では復旧しない。起こし直しでは上流から順に READY へ落として
    /// 送出を止め、下流から順に親状態へ戻すことで、上流が送出を始める時点で下流が
    /// 受入可能になっていることを保証する。
    pub fn restart_branch_flow(&self) -> Result<(), String> {
        if self.pipeline.is_none() {
            return Err(String::from("パイプラインに接続していない枝は起こし直せない"));
        }
        for element in &self.elements {
            element
                .set_state(gstreamer::State::Ready)
                .map_err(|error| error.to_string())?;
        }
        for element in self.elements.iter().rev() {
            element
                .sync_state_with_parent()
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Returns the pipeline that currently owns this stream's elements.
    pub fn attached_pipeline(&self) -> Option<gstreamer::Pipeline> {
        self.pipeline.clone()
    }

    /// Creates a GStreamer-only branch over this track's shared source.
    ///
    /// Unlike MediaStreamTrack.clone(), the branch mirrors this track's enabled and ended
    /// state. It is intended for a backend consumer that needs separate GStreamer elements
    /// because the original track is already attached to another pipeline.
    pub fn try_clone_branch(&self) -> Result<MediaStreamId, String> {
        let branch = self
            .branch
            .as_ref()
            .ok_or_else(|| String::from("独立したトラック枝ではない"))?;
        if branch.stopped {
            return Err(String::from("終了したトラック枝は複製できない"));
        }
        Self::create_branch_with_controls(
            branch.source.clone(),
            self.type_,
            Some(branch.controls.clone()),
        )
    }

    pub fn attach_to_pipeline(&mut self, pipeline: &gstreamer::Pipeline) {
        assert!(self.pipeline.is_none());
        let elements: Vec<_> = self.elements.iter().collect();
        pipeline.add_many(&elements[..]).unwrap();
        gstreamer::Element::link_many(&elements[..]).unwrap();
        // 下流から順に親状態へ揃える。上流から揃えると、上流の送出タスクがまだ
        // NULL/READY の下流へ送出して止まり、その後の同期では復旧しないことがある。
        // 下流から揃えれば、上流が送出を始める時点で下流は受入可能になっている。
        // 親が PLAYING でない場合（初期接続など）は順序による差は無い。
        for element in elements.iter().rev() {
            element.sync_state_with_parent().unwrap();
        }
        self.pipeline = Some(pipeline.clone());
    }

    pub fn pipeline_or_new(&mut self) -> gstreamer::Pipeline {
        match self.pipeline {
            Some(ref pipeline) => pipeline.clone(),
            _ => {
                let pipeline =
                    gstreamer::Pipeline::with_name("gstreamermediastream fresh pipeline");
                let clock = gstreamer::SystemClock::obtain();
                pipeline.set_start_time(gstreamer::ClockTime::NONE);
                pipeline.set_base_time(*BACKEND_BASE_TIME);
                pipeline.use_clock(Some(&clock));
                self.attach_to_pipeline(&pipeline);
                pipeline
            },
        }
    }

    pub fn try_create_video() -> Result<MediaStreamId, String> {
        let videotestsrc = gstreamer::ElementFactory::make("videotestsrc")
            .property_from_str("pattern", "ball")
            .property("is-live", true)
            .build()
            .map_err(|error| error.to_string())?;
        Self::try_create_video_from(videotestsrc)
    }

    pub fn create_video() -> MediaStreamId {
        Self::try_create_video().expect("合成映像トラックを作れない")
    }

    /// Attaches encoding adapters to the stream, returning the source element when successful.
    pub fn encoded(&mut self) -> Result<gstreamer::Element, BoolError> {
        let pipeline = self
            .pipeline
            .as_ref()
            .expect("GStreamerMediaStream::encoded() should not be called without a pipeline");
        let src = self.src_element();

        let capsfilter = gstreamer::ElementFactory::make("capsfilter")
            .property("caps", self.caps())
            .build()?;
        match self.type_ {
            MediaStreamType::Video => {
                let vp8enc = gstreamer::ElementFactory::make("vp8enc")
                    .property("deadline", 1i64)
                    .property_from_str("error-resilient", "default")
                    .property("cpu-used", -16i32)
                    .property("lag-in-frames", 0i32)
                    .build()?;

                let rtpvp8pay = gstreamer::ElementFactory::make("rtpvp8pay")
                    .property_from_str("picture-id-mode", "15-bit")
                    .property("mtu", 1200u32)
                    .build()?;
                let queue2 = gstreamer::ElementFactory::make("queue").build()?;

                pipeline.add_many([&vp8enc, &rtpvp8pay, &queue2, &capsfilter])?;
                gstreamer::Element::link_many([&src, &vp8enc, &rtpvp8pay, &queue2, &capsfilter])?;
                vp8enc.sync_state_with_parent()?;
                rtpvp8pay.sync_state_with_parent()?;
                queue2.sync_state_with_parent()?;
                capsfilter.sync_state_with_parent()?;
                Ok(capsfilter)
            },
            MediaStreamType::Audio => {
                let opusenc = gstreamer::ElementFactory::make("opusenc").build()?;
                let rtpopuspay = gstreamer::ElementFactory::make("rtpopuspay")
                    .property("mtu", 1200u32)
                    .build()?;
                let queue3 = gstreamer::ElementFactory::make("queue").build()?;
                pipeline.add_many([&opusenc, &rtpopuspay, &queue3, &capsfilter])?;
                gstreamer::Element::link_many([&src, &opusenc, &rtpopuspay, &queue3, &capsfilter])?;
                opusenc.sync_state_with_parent()?;
                rtpopuspay.sync_state_with_parent()?;
                queue3.sync_state_with_parent()?;
                Ok(capsfilter)
            },
        }
    }

    /// 再生用の共有 encoder 出力と pipeline を返す。ストリームごとに1本だけ作り、
    /// 2人目以降の player は同じ tee から proxysink を生やす。元 branch が
    /// WebRTC 送出などで既に encoder に link 済みなら、player 専用 branch を clone する。
    pub fn encoded_for_playback(
        &mut self,
    ) -> Result<(gstreamer::Element, gstreamer::Pipeline), String> {
        if let Some((tee, pipeline)) = self.playback_output.as_ref() {
            info!(
                "encoded_for_playback: 既存の共有出力を再利用する (tee={}, pipeline={})",
                tee.name(),
                pipeline.name()
            );
            return Ok((tee.clone(), pipeline.clone()));
        }

        let source_is_linked = self
            .src_element()
            .static_pad("src")
            .map(|pad| pad.is_linked())
            .unwrap_or(false);
        let (pipeline, capsfilter) = if source_is_linked {
            let clone_id = self.try_clone_branch()?;
            let clone = get_stream(&clone_id).ok_or_else(|| {
                String::from("再生用 branch clone が registry に無い")
            })?;
            let mut clone = clone.lock().unwrap();
            let clone = clone
                .as_mut_any()
                .downcast_mut::<GStreamerMediaStream>()
                .ok_or_else(|| String::from("再生用 branch clone が GStreamer 実装でない"))?;
            let pipeline = clone.pipeline_or_new();
            let capsfilter = clone.encoded().map_err(|error| error.to_string())?;
            info!(
                "encoded_for_playback: player 用 branch clone を作成した (id={})",
                clone_id.id()
            );
            self.playback_clone = Some(clone_id);
            (pipeline, capsfilter)
        } else {
            let pipeline = self.pipeline_or_new();
            let capsfilter = self.encoded().map_err(|error| error.to_string())?;
            (pipeline, capsfilter)
        };
        info!(
            "encoded_for_playback: encoded() output={} caps={:?}",
            capsfilter.name(),
            capsfilter
                .static_pad("src")
                .and_then(|pad| pad.current_caps())
        );

        let tee = gstreamer::ElementFactory::make("tee")
            .property("allow-not-linked", true)
            .build()
            .map_err(|error| error.to_string())?;
        pipeline.add(&tee).map_err(|error| error.to_string())?;
        gstreamer::Element::link_many([&capsfilter, &tee])
            .map_err(|error| error.to_string())?;
        tee.sync_state_with_parent()
            .map_err(|error| error.to_string())?;
        info!(
            "encoded_for_playback: 共有出力を作成した (tee={}, pipeline={})",
            tee.name(),
            pipeline.name()
        );
        self.playback_output = Some((tee.clone(), pipeline.clone()));
        Ok((tee, pipeline))
    }

    pub fn try_create_video_from(source: gstreamer::Element) -> Result<MediaStreamId, String> {
        Self::create_shared_from(source, MediaStreamType::Video)
    }

    /// 画面共有など、ストリーム終了時に外部資源を片付ける必要がある入力向け。
    /// `cleanup` は最後の枝の停止時または共有入力の破棄時に一度だけ呼ばれる。
    pub fn try_create_video_from_with_cleanup(
        source: gstreamer::Element,
        cleanup: Box<dyn FnOnce() + Send>,
    ) -> Result<MediaStreamId, String> {
        Self::create_shared_from_with_cleanup(
            source,
            MediaStreamType::Video,
            None,
            Some(cleanup),
        )
    }

    pub fn try_create_video_from_device(
        source: gstreamer::Element,
        device_id: Option<String>,
    ) -> Result<MediaStreamId, String> {
        Self::create_shared_from_with_device(source, MediaStreamType::Video, device_id)
    }

    pub fn try_create_audio() -> Result<MediaStreamId, String> {
        let audiotestsrc = gstreamer::ElementFactory::make("audiotestsrc")
            .property_from_str("wave", "sine")
            .property("is-live", true)
            .build()
            .map_err(|error| error.to_string())?;

        Self::try_create_audio_from(audiotestsrc)
    }

    pub fn create_audio() -> MediaStreamId {
        Self::try_create_audio().expect("合成音声トラックを作れない")
    }

    pub fn try_create_audio_from(source: gstreamer::Element) -> Result<MediaStreamId, String> {
        Self::create_shared_from(source, MediaStreamType::Audio)
    }

    pub fn try_create_audio_from_device(
        source: gstreamer::Element,
        device_id: Option<String>,
    ) -> Result<MediaStreamId, String> {
        Self::create_shared_from_with_device(source, MediaStreamType::Audio, device_id)
    }

    pub fn create_proxy(ty: MediaStreamType) -> (MediaStreamId, GstreamerMediaSocket) {
        Self::try_create_proxy(ty).expect("GStreamer のメディアプロキシを作れない")
    }

    pub fn try_create_proxy(
        ty: MediaStreamType,
    ) -> Result<(MediaStreamId, GstreamerMediaSocket), String> {
        let proxy_sink = gstreamer::ElementFactory::make("proxysink")
            .build()
            .map_err(|error| error.to_string())?;
        let proxy_src = gstreamer::ElementFactory::make("proxysrc")
            .property("proxysink", &proxy_sink)
            .build()
            .map_err(|error| error.to_string())?;
        let stream = match ty {
            MediaStreamType::Audio => Self::try_create_audio_from(proxy_src)?,
            MediaStreamType::Video => Self::try_create_video_from(proxy_src)?,
        };

        Ok((stream, GstreamerMediaSocket { proxy_sink }))
    }
}

impl Drop for GStreamerMediaStream {
    fn drop(&mut self) {
        if let Some(clone) = self.playback_clone.take() {
            if let Err(error) = stop_stream(&clone) {
                warn!("再生用 branch clone を止められない: {error}");
            }
            unregister_stream(&clone);
            self.playback_output = None;
        }
        if let Some(ref id) = self.id {
            unregister_stream(id);
        }
    }
}

#[derive(Default)]
pub struct MediaSink {
    streams: Vec<Arc<Mutex<dyn MediaStream>>>,
}

impl MediaOutput for MediaSink {
    fn add_stream(&mut self, stream: &MediaStreamId) {
        let stream = get_stream(stream).expect("Media streams registry does not contain such ID");
        {
            let mut stream = stream.lock().unwrap();
            let stream = stream
                .as_mut_any()
                .downcast_mut::<GStreamerMediaStream>()
                .unwrap();
            let pipeline = stream.pipeline_or_new();
            let last_element = stream.elements.last();
            let last_element = last_element.as_ref().unwrap();
            let sink = match stream.type_ {
                MediaStreamType::Audio => "autoaudiosink",
                MediaStreamType::Video => "autovideosink",
            };
            let sink = gstreamer::ElementFactory::make(sink).build().unwrap();
            pipeline.add(&sink).unwrap();
            gstreamer::Element::link_many(&[last_element, &sink][..]).unwrap();

            pipeline.set_state(gstreamer::State::Playing).unwrap();
            sink.sync_state_with_parent().unwrap();
        }
        self.streams.push(stream.clone());
    }
}

pub struct GstreamerMediaSocket {
    proxy_sink: gstreamer::Element,
}

impl GstreamerMediaSocket {
    pub fn proxy_sink(&self) -> &gstreamer::Element {
        &self.proxy_sink
    }
}

impl MediaSocket for GstreamerMediaSocket {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
#[path = "media_stream_tests.rs"]
mod tests;
