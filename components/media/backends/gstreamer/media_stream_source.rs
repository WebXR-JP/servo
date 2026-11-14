/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use glib::subclass::prelude::*;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use gstreamer_base::UniqueFlowCombiner;
use log::{info, warn};
use servo_media_player::PlayerError;
use servo_media_streams::registry::MediaStreamId;
use servo_media_streams::{MediaStream, MediaStreamType};
use url::Url;

use crate::media_stream::{GStreamerMediaStream, RTP_CAPS_OPUS, RTP_CAPS_VP8};

// Implementation sub-module of the GObject
mod imp {

    use super::*;

    static AUDIO_SRC_PAD_TEMPLATE: LazyLock<gstreamer::PadTemplate> = LazyLock::new(|| {
        gstreamer::PadTemplate::new(
            "audio_src",
            gstreamer::PadDirection::Src,
            gstreamer::PadPresence::Sometimes,
            &RTP_CAPS_OPUS,
        )
        .expect("Could not create audio src pad template")
    });

    static VIDEO_SRC_PAD_TEMPLATE: LazyLock<gstreamer::PadTemplate> = LazyLock::new(|| {
        gstreamer::PadTemplate::new(
            "video_src",
            gstreamer::PadDirection::Src,
            gstreamer::PadPresence::Sometimes,
            &RTP_CAPS_VP8,
        )
        .expect("Could not create video src pad template")
    });

    pub struct ServoMediaStreamSrc {
        cat: gstreamer::DebugCategory,
        audio_proxysrc: gstreamer::Element,
        audio_srcpad: gstreamer::GhostPad,
        video_proxysrc: gstreamer::Element,
        video_srcpad: gstreamer::GhostPad,
        flow_combiner: Arc<Mutex<UniqueFlowCombiner>>,
        has_audio_stream: Arc<AtomicBool>,
        has_video_stream: Arc<AtomicBool>,
        /// 現在この src に設定済みの音声ストリーム。同じストリームの
        /// 再設定を no-op にする。
        audio_stream: Arc<Mutex<Option<MediaStreamId>>>,
        /// 現在この src に設定済みの映像ストリーム。同じストリームの
        /// 再設定を no-op にする。
        video_stream: Arc<Mutex<Option<MediaStreamId>>>,
    }

    impl ServoMediaStreamSrc {
        pub fn set_stream(
            &self,
            stream: &mut GStreamerMediaStream,
            src: &gstreamer::Element,
            only_stream: bool,
        ) -> Result<(), PlayerError> {
            // XXXferjm the current design limits the number of streams to one
            // per type. This fulfills the basic use case for WebRTC, but we should
            // implement support for multiple streams per type at some point, which
            // likely involves encoding and muxing all streams of the same type
            // in a single stream.

            gstreamer::log!(self.cat, "Setting stream");

            let stream_id = stream.id();
            let (current, src_pad) = match stream.ty() {
                MediaStreamType::Audio => {
                    (*self.audio_stream.lock().unwrap(), &self.audio_srcpad)
                },
                MediaStreamType::Video => {
                    (*self.video_stream.lock().unwrap(), &self.video_srcpad)
                },
            };
            info!(
                "ServoMediaStreamSrc::set_stream: src_ptr={:p} type={:?} only_stream={} \
                 caps={:?} src_element={} stream_id={:?} current={:?}",
                self as *const Self,
                stream.ty(),
                only_stream,
                stream.caps(),
                stream.src_element().name(),
                stream_id.map(|id| id.id().to_string()),
                current.map(|id| id.id().to_string())
            );

            // 同じストリームが同じ src に二度設定された場合は、encoded() で
            // encoder 枝を二重に作らず no-op にする。media element の srcObject
            // 再設定などで同じ track が再度渡される経路がある。
            if let Some(stream_id) = stream_id
                && current == Some(stream_id)
                && src_pad.parent().is_some()
            {
                info!(
                    "ServoMediaStreamSrc::set_stream: stream id={} is already \
                     configured on {}; skipping encoded()",
                    stream_id.id(),
                    src_pad.name()
                );
                return Ok(());
            }

            // Append a proxysink to the media stream pipeline.
            let (last_element, playback_pipeline) =
                stream.encoded_for_playback().map_err(|error| {
                    warn!(
                        "ServoMediaStreamSrc::set_stream: encoded_for_playback() failed: {error}"
                    );
                    PlayerError::SetStreamFailed
                })?;
            let encoded_caps = last_element
                .static_pad("src")
                .and_then(|pad| pad.current_caps());
            info!(
                "ServoMediaStreamSrc::set_stream: playback output (tee)={} pipeline={} caps={:?}",
                last_element.name(),
                playback_pipeline.name(),
                encoded_caps
            );
            let sink = gstreamer::ElementFactory::make("proxysink")
                .build()
                .map_err(|error| {
                    warn!(
                        "ServoMediaStreamSrc::set_stream: proxysink creation failed: {error}"
                    );
                    PlayerError::SetStreamFailed
                })?;
            playback_pipeline.add(&sink).map_err(|error| {
                warn!("ServoMediaStreamSrc::set_stream: pipeline.add(proxysink) failed: {error}");
                PlayerError::SetStreamFailed
            })?;
            gstreamer::Element::link_many(&[&last_element, &sink][..]).map_err(|error| {
                warn!("ServoMediaStreamSrc::set_stream: link_many(last_element, proxysink) failed: {error}");
                PlayerError::SetStreamFailed
            })?;

            // Create the appropriate proxysrc depending on the stream type
            // and connect the media stream proxysink to it.
            self.setup_proxy_src(stream.ty(), &sink, src, only_stream)?;

            sink.sync_state_with_parent().map_err(|error| {
                warn!(
                    "ServoMediaStreamSrc::set_stream: proxysink sync_state_with_parent failed: {error}"
                );
                PlayerError::SetStreamFailed
            })?;
            playback_pipeline
                .set_state(gstreamer::State::Playing)
                .map_err(|error| {
                    warn!(
                        "ServoMediaStreamSrc::set_stream: pipeline.set_state(Playing) failed: {error}"
                    );
                    PlayerError::SetStreamFailed
                })?;

            if let Some(stream_id) = stream.id() {
                match stream.ty() {
                    MediaStreamType::Audio => {
                        *self.audio_stream.lock().unwrap() = Some(stream_id);
                    },
                    MediaStreamType::Video => {
                        *self.video_stream.lock().unwrap() = Some(stream_id);
                    },
                }
            }

            Ok(())
        }

        fn setup_proxy_src(
            &self,
            stream_type: MediaStreamType,
            sink: &gstreamer::Element,
            src: &gstreamer::Element,
            only_stream: bool,
        ) -> Result<(), PlayerError> {
            let (proxysrc, src_pad, no_more_pads) = match stream_type {
                MediaStreamType::Audio => {
                    self.has_audio_stream.store(true, Ordering::Relaxed);
                    (
                        &self.audio_proxysrc,
                        &self.audio_srcpad,
                        self.has_video_stream.load(Ordering::Relaxed),
                    )
                },
                MediaStreamType::Video => {
                    self.has_video_stream.store(true, Ordering::Relaxed);
                    (
                        &self.video_proxysrc,
                        &self.video_srcpad,
                        self.has_audio_stream.load(Ordering::Relaxed),
                    )
                },
            };
            proxysrc.set_property("proxysink", sink);

            // Add proxysrc to bin
            let Some(bin) = src.downcast_ref::<gstreamer::Bin>() else {
                warn!("ServoMediaStreamSrc::setup_proxy_src: src is not a Bin");
                return Err(PlayerError::SetStreamFailed);
            };
            info!(
                "ServoMediaStreamSrc::setup_proxy_src: src_ptr={:p} type={:?} \
                 only_stream={} proxysrc={} src_pad={} parent={:?} is_linked={}",
                self as *const Self,
                stream_type,
                only_stream,
                proxysrc.name(),
                src_pad.name(),
                src_pad.parent().map(|parent| parent.name().to_string()),
                src_pad.is_linked()
            );
            if let Err(error) = bin.add(proxysrc) {
                let already_in_bin =
                    proxysrc.parent().as_ref() == Some(bin.upcast_ref::<gstreamer::Object>());
                if already_in_bin {
                    info!(
                        "ServoMediaStreamSrc::setup_proxy_src: proxysrc is already in this bin"
                    );
                } else {
                    warn!(
                        "ServoMediaStreamSrc::setup_proxy_src: bin.add(proxysrc) failed: {error}"
                    );
                    return Err(PlayerError::SetStreamFailed);
                }
            }

            let target_pad = match proxysrc.static_pad("src") {
                Some(pad) => pad,
                None => {
                    warn!(
                        "ServoMediaStreamSrc::setup_proxy_src: proxysrc has no static src pad"
                    );
                    return Err(PlayerError::SetStreamFailed);
                },
            };
            if let Err(error) = src_pad.set_target(Some(&target_pad)) {
                warn!("ServoMediaStreamSrc::setup_proxy_src: set_target failed: {error}");
                return Err(PlayerError::SetStreamFailed);
            }

            if let Err(error) = src.add_pad(src_pad) {
                // 以降の状態同期は試す（正常経路の挙動を変えない）。
                warn!("ServoMediaStreamSrc::setup_proxy_src: add_pad failed: {error}");
            }
            src.set_element_flags(gstreamer::ElementFlags::SOURCE);

            match src_pad.internal() {
                Some(proxy_pad) => self
                    .flow_combiner
                    .lock()
                    .unwrap()
                    .add_pad(&proxy_pad),
                None => {
                    warn!("ServoMediaStreamSrc::setup_proxy_src: src_pad.internal() is None")
                },
            }
            if let Err(error) = src_pad.set_active(true) {
                warn!("ServoMediaStreamSrc::setup_proxy_src: set_active failed: {error}");
            }

            if let Err(error) = src.sync_state_with_parent() {
                warn!(
                    "ServoMediaStreamSrc::setup_proxy_src: sync_state_with_parent failed: {error}"
                );
            }

            if no_more_pads || only_stream {
                src.no_more_pads();
            }

            Ok(())
        }
    }

    // Basic declaration of our type for the GObject type system.
    #[glib::object_subclass]
    impl ObjectSubclass for ServoMediaStreamSrc {
        const NAME: &'static str = "ServoMediaStreamSrc";
        type Type = super::ServoMediaStreamSrc;
        type ParentType = gstreamer::Bin;
        type Interfaces = (gstreamer::URIHandler,);

        // Called once at the very beginning of instantiation of each instance and
        // creates the data structure that contains all our state
        fn with_class(_klass: &Self::Class) -> Self {
            let flow_combiner = Arc::new(Mutex::new(UniqueFlowCombiner::new()));

            fn create_ghost_pad_with_template(
                name: &str,
                pad_template: &gstreamer::PadTemplate,
                flow_combiner: Arc<Mutex<UniqueFlowCombiner>>,
            ) -> gstreamer::GhostPad {
                gstreamer::GhostPad::builder_from_template(pad_template)
                    .name(name)
                    .chain_function({
                        move |pad, parent, buffer| {
                            let chain_result =
                                gstreamer::ProxyPad::chain_default(pad, parent, buffer);
                            let result = flow_combiner
                                .lock()
                                .unwrap()
                                .update_pad_flow(pad, chain_result);
                            if result == Err(gstreamer::FlowError::Flushing) {
                                return chain_result;
                            }
                            result
                        }
                    })
                    .build()
            }

            let audio_proxysrc = gstreamer::ElementFactory::make("proxysrc")
                .build()
                .expect("Could not create proxysrc element");
            let audio_srcpad = create_ghost_pad_with_template(
                "audio_src",
                &AUDIO_SRC_PAD_TEMPLATE,
                flow_combiner.clone(),
            );

            let video_proxysrc = gstreamer::ElementFactory::make("proxysrc")
                .build()
                .expect("Could not create proxysrc element");
            let video_srcpad = create_ghost_pad_with_template(
                "video_src",
                &VIDEO_SRC_PAD_TEMPLATE,
                flow_combiner.clone(),
            );

            Self {
                cat: gstreamer::DebugCategory::new(
                    "servomediastreamsrc",
                    gstreamer::DebugColorFlags::empty(),
                    Some("Servo media stream source"),
                ),
                audio_proxysrc,
                audio_srcpad,
                video_proxysrc,
                video_srcpad,
                flow_combiner,
                has_video_stream: Arc::new(AtomicBool::new(false)),
                has_audio_stream: Arc::new(AtomicBool::new(false)),
                audio_stream: Arc::new(Mutex::new(None)),
                video_stream: Arc::new(Mutex::new(None)),
            }
        }
    }

    // The ObjectImpl trait provides the setters/getters for GObject properties.
    // Here we need to provide the values that are internally stored back to the
    // caller, or store whatever new value the caller is providing.
    //
    // This maps between the GObject properties and our internal storage of the
    // corresponding values of the properties.
    impl ObjectImpl for ServoMediaStreamSrc {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                vec![
                    // Let playbin3 know we are a live source.
                    glib::ParamSpecBoolean::builder("is-live")
                        .nick("Is Live")
                        .blurb("Let playbin3 know we are a live source")
                        .default_value(true)
                        .readwrite()
                        .build(),
                ]
            });

            &PROPERTIES
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "is-live" => true.to_value(),
                _ => unimplemented!(),
            }
        }
    }

    impl GstObjectImpl for ServoMediaStreamSrc {}

    // Implementation of gstreamer::Element virtual methods
    impl ElementImpl for ServoMediaStreamSrc {
        fn metadata() -> Option<&'static gstreamer::subclass::ElementMetadata> {
            static ELEMENT_METADATA: LazyLock<gstreamer::subclass::ElementMetadata> =
                LazyLock::new(|| {
                    gstreamer::subclass::ElementMetadata::new(
                        "Servo Media Stream Source",
                        "Source/Audio/Video",
                        "Feed player with media stream data",
                        "Servo developers",
                    )
                });

            Some(&*ELEMENT_METADATA)
        }

        fn pad_templates() -> &'static [gstreamer::PadTemplate] {
            static PAD_TEMPLATES: LazyLock<Vec<gstreamer::PadTemplate>> = LazyLock::new(|| {
                // Add pad templates for our audio and video source pads.
                // These are later used for actually creating the pads and beforehand
                // already provide information to GStreamer about all possible
                // pads that could exist for this type.
                vec![
                    AUDIO_SRC_PAD_TEMPLATE.clone(),
                    VIDEO_SRC_PAD_TEMPLATE.clone(),
                ]
            });

            PAD_TEMPLATES.as_ref()
        }
    }

    // Implementation of gstreamer::Bin virtual methods
    impl BinImpl for ServoMediaStreamSrc {}

    impl URIHandlerImpl for ServoMediaStreamSrc {
        const URI_TYPE: gstreamer::URIType = gstreamer::URIType::Src;

        fn protocols() -> &'static [&'static str] {
            &["mediastream"]
        }

        fn uri(&self) -> Option<String> {
            Some("mediastream://".to_string())
        }

        fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
            if let Ok(uri) = Url::parse(uri)
                && uri.scheme() == "mediastream"
            {
                return Ok(());
            }
            Err(glib::Error::new(
                gstreamer::URIError::BadUri,
                format!("Invalid URI '{:?}'", uri,).as_str(),
            ))
        }
    }
}

// Public part of the ServoMediaStreamSrc type. This behaves like a normal
// GObject binding
glib::wrapper! {
    pub struct ServoMediaStreamSrc(ObjectSubclass<imp::ServoMediaStreamSrc>)
        @extends gstreamer::Bin, gstreamer::Element, gstreamer::Object, @implements gstreamer::URIHandler;
}

unsafe impl Send for ServoMediaStreamSrc {}
unsafe impl Sync for ServoMediaStreamSrc {}

impl ServoMediaStreamSrc {
    pub fn set_stream(
        &self,
        stream: &mut GStreamerMediaStream,
        only_stream: bool,
    ) -> Result<(), PlayerError> {
        self.imp()
            .set_stream(stream, self.upcast_ref::<gstreamer::Element>(), only_stream)
    }
}

// Registers the type for our element, and then registers in GStreamer
// under the name "servomediastreamsrc" for being able to instantiate it via e.g.
// gstreamer::ElementFactory::make().
pub fn register_servo_media_stream_src() -> Result<(), glib::BoolError> {
    gstreamer::Element::register(
        None,
        "servomediastreamsrc",
        gstreamer::Rank::NONE,
        ServoMediaStreamSrc::static_type(),
    )
}
