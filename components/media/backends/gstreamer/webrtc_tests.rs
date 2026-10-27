/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer::prelude::*;
use gstreamer_app::AppSink;
use servo_media_streams::MediaStreamType;
use servo_media_streams::registry::{MediaStreamId, get_stream};
use servo_media_webrtc::{
    BundlePolicy, ConnectionState, DataChannelInit, IceCandidate, IceConfiguration,
    IceTransportPolicy, OutgoingTrack, SessionDescription, TransceiverDirection, TransceiverKind,
    WebRtcController, WebRtcSignaller,
};

use crate::GStreamerBackend;
use crate::media_stream::GStreamerMediaStream;

const TIMEOUT: Duration = Duration::from_secs(20);

enum Event {
    Connected(&'static str),
    Failed(&'static str),
    IncomingAudio(
        &'static str,
        MediaStreamId,
        servo_media_webrtc::RemoteTrackMetadata,
    ),
}

struct LoopbackSignaller {
    name: &'static str,
    peer: Arc<Mutex<Option<WebRtcController>>>,
    events: Sender<Event>,
}

impl WebRtcSignaller for LoopbackSignaller {
    fn on_ice_candidate(&self, _: &WebRtcController, candidate: IceCandidate) {
        if let Some(peer) = self.peer.lock().unwrap().as_ref() {
            peer.add_ice_candidate(candidate);
        }
    }

    fn on_negotiation_needed(&self, _: &WebRtcController) {}

    fn close(&self) {}

    fn on_add_stream(
        &self,
        stream: &MediaStreamId,
        ty: MediaStreamType,
        metadata: servo_media_webrtc::RemoteTrackMetadata,
    ) {
        if ty == MediaStreamType::Audio {
            let _ = self
                .events
                .send(Event::IncomingAudio(self.name, *stream, metadata));
        }
    }

    fn update_connection_state(&self, state: ConnectionState) {
        match state {
            ConnectionState::Connected => {
                let _ = self.events.send(Event::Connected(self.name));
            },
            ConnectionState::Failed => {
                let _ = self.events.send(Event::Failed(self.name));
            },
            _ => {},
        }
    }
}

fn wait_for_event(receiver: &Receiver<Event>) -> Event {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("WebRTC loopback exceeded its timeout");
        return receiver
            .recv_timeout(remaining)
            .expect("WebRTC loopback did not produce the expected event");
    }
}

struct LoopbackPeers {
    offer: WebRtcController,
    answer: WebRtcController,
}

struct IncomingAudioConsumer {
    pipeline: gstreamer::Pipeline,
    sink: AppSink,
}

impl IncomingAudioConsumer {
    fn attach(stream_id: MediaStreamId) -> Self {
        let stream = get_stream(&stream_id).expect("incoming audio track should be registered");
        let mut stream = stream.lock().unwrap();
        let stream = stream
            .as_mut_any()
            .downcast_mut::<GStreamerMediaStream>()
            .expect("incoming audio track should use the GStreamer backend");
        let pipeline = stream.pipeline_or_new();
        let output = stream.src_element();
        let convert = gstreamer::ElementFactory::make("audioconvert")
            .build()
            .expect("incoming audio converter should be available");
        let capsfilter = gstreamer::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gstreamer::Caps::builder("audio/x-raw")
                    .field("format", "F32LE")
                    .field("layout", "interleaved")
                    .field("rate", 48_000i32)
                    .field("channels", 2i32)
                    .build(),
            )
            .build()
            .expect("incoming PCM caps filter should be available");
        let sink = gstreamer::ElementFactory::make("appsink")
            .property("async", false)
            .property("sync", false)
            .property("max-buffers", 4u32)
            .property("drop", true)
            .build()
            .expect("incoming PCM appsink should be available")
            .downcast::<AppSink>()
            .expect("incoming PCM sink should have the AppSink type");

        pipeline
            .add_many([&convert, &capsfilter, sink.upcast_ref()])
            .expect("incoming PCM consumer should be added to its pipeline");
        gstreamer::Element::link_many([&output, &convert, &capsfilter, sink.upcast_ref()])
            .expect("incoming audio track should link to its PCM consumer");
        pipeline
            .set_state(gstreamer::State::Playing)
            .expect("incoming PCM consumer pipeline should start");

        Self { pipeline, sink }
    }

    fn receive_nonzero_pcm(&self) -> f64 {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("WebRTC loopback did not deliver nonzero PCM samples");
            let sample = self
                .sink
                .try_pull_sample(gstreamer::ClockTime::from_nseconds(
                    remaining.as_nanos() as u64
                ))
                .expect("WebRTC loopback did not deliver decoded PCM");
            let buffer = sample
                .buffer()
                .expect("decoded PCM sample should contain a buffer");
            let bytes = buffer
                .map_readable()
                .expect("decoded PCM buffer should be readable");
            let samples: Vec<_> = bytes
                .as_slice()
                .chunks_exact(std::mem::size_of::<f32>())
                .map(|sample| f32::from_le_bytes(sample.try_into().unwrap()))
                .collect();
            let rms = (samples
                .iter()
                .map(|sample| f64::from(*sample).powi(2))
                .sum::<f64>()
                / samples.len() as f64)
                .sqrt();
            if rms > 0.01 {
                return rms;
            }
        }
    }
}

impl Drop for IncomingAudioConsumer {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}

impl Drop for LoopbackPeers {
    fn drop(&mut self) {
        self.offer.quit();
        self.answer.quit();
    }
}

fn wait_for_description(
    controller: &WebRtcController,
    create: impl FnOnce(&WebRtcController, Box<dyn FnOnce(SessionDescription) + Send + 'static>),
) -> SessionDescription {
    let (sender, receiver) = channel();
    create(
        controller,
        Box::new(move |description| {
            sender.send(description).expect("test receiver disappeared");
        }),
    );
    receiver
        .recv_timeout(TIMEOUT)
        .expect("WebRTC did not produce an SDP description")
}

fn set_description(controller: &WebRtcController, description: SessionDescription, remote: bool) {
    let (sender, receiver) = channel();
    let completion = Box::new(move || sender.send(()).expect("test receiver disappeared"));
    if remote {
        controller.set_remote_description(description, completion);
    } else {
        controller.set_local_description(description, completion);
    }
    receiver
        .recv_timeout(TIMEOUT)
        .expect("WebRTC did not apply its SDP description");
}

#[test]
fn current_remote_answer_commits_its_sdp_metadata() {
    gstreamer::init().expect("GStreamer must initialize for SDP metadata tests");
    let metadata = super::RemoteMLineMetadata {
        stream_ids: vec!["lk-stream".into()],
        track_id: Some("lk-track".into()),
        mid: Some("audio0".into()),
        kind: Some(TransceiverKind::Audio),
        ssrcs: vec![],
    };
    let mut pending_info = vec![super::MLineInfo {
        caps: gstreamer::Caps::new_empty(),
        is_used: false,
        payload: 102,
    }];
    let mut current_info = Vec::new();
    let mut pending_metadata = vec![metadata.clone()];
    let current_metadata = Mutex::new(Vec::new());

    assert!(super::commit_remote_mline_info(
        servo_media_webrtc::DescriptionType::Remote,
        servo_media_webrtc::SdpType::Answer,
        4,
        4,
        &mut pending_info,
        &mut current_info,
        &mut pending_metadata,
        &current_metadata,
    ));
    assert!(pending_info.is_empty());
    assert_eq!(current_info.len(), 1);
    assert!(pending_metadata.is_empty());
    assert_eq!(*current_metadata.lock().unwrap(), vec![metadata]);

    let mut pending_info = vec![super::MLineInfo {
        caps: gstreamer::Caps::new_empty(),
        is_used: false,
        payload: 111,
    }];
    let mut pending_metadata = vec![super::RemoteMLineMetadata {
        stream_ids: vec!["stale-stream".into()],
        track_id: Some("stale-track".into()),
        mid: Some("audio1".into()),
        kind: Some(TransceiverKind::Audio),
        ssrcs: vec![],
    }];
    assert!(!super::commit_remote_mline_info(
        servo_media_webrtc::DescriptionType::Remote,
        servo_media_webrtc::SdpType::Answer,
        3,
        4,
        &mut pending_info,
        &mut current_info,
        &mut pending_metadata,
        &current_metadata,
    ));
    assert_eq!(pending_info.len(), 1);
    assert_eq!(pending_metadata[0].track_id.as_deref(), Some("stale-track"));
}

#[test]
fn remote_pranswer_and_local_answer_do_not_commit_pending_metadata() {
    gstreamer::init().expect("GStreamer must initialize for SDP metadata tests");
    let make_state = || {
        (
            vec![super::MLineInfo {
                caps: gstreamer::Caps::new_empty(),
                is_used: false,
                payload: 111,
            }],
            Vec::new(),
            vec![super::RemoteMLineMetadata {
                stream_ids: vec!["stream".into()],
                track_id: Some("track".into()),
                mid: Some("audio0".into()),
                kind: Some(TransceiverKind::Audio),
                ssrcs: vec![],
            }],
            Mutex::new(Vec::new()),
        )
    };
    for (description_type, ty) in [
        (
            servo_media_webrtc::DescriptionType::Remote,
            servo_media_webrtc::SdpType::Pranswer,
        ),
        (
            servo_media_webrtc::DescriptionType::Local,
            servo_media_webrtc::SdpType::Answer,
        ),
    ] {
        let (mut pending, mut current, mut metadata, committed) = make_state();
        assert!(!super::commit_remote_mline_info(
            description_type,
            ty,
            1,
            1,
            &mut pending,
            &mut current,
            &mut metadata,
            &committed,
        ));
        assert_eq!(pending.len(), 1);
        assert!(current.is_empty());
        assert_eq!(metadata[0].track_id.as_deref(), Some("track"));
        assert!(committed.lock().unwrap().is_empty());
    }
}

#[test]
fn incoming_rtp_ssrc_overrides_a_mismatched_mid() {
    gstreamer::init().expect("GStreamer must initialize for SDP metadata tests");
    let sdp = super::gstreamer_sdp::SDPMessage::parse_buffer(
        b"v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:audio0\r\na=msid:lk-stream lk-track\r\na=ssrc:3503253273 msid:lk-stream lk-track\r\nm=audio 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:audio1\r\n",
    )
    .expect("test SDP must parse");
    let remote_mlines = sdp
        .medias()
        .map(super::remote_mline_metadata)
        .collect::<Result<Vec<_>, _>>()
        .expect("test SDP metadata must parse");

    let stream_start_ssrc =
        super::ssrc_from_webrtc_stream_id("c0a8d8db3e886da0abe72b857d27570a/3503253273/96");
    let (mline_index, mid) =
        super::incoming_mline_for_rtp(&remote_mlines, stream_start_ssrc, Some("audio1".into()), 1)
            .expect("the SSRC must select its advertised m-line");

    assert_eq!(mline_index, 0);
    assert_eq!(mid.as_deref(), Some("audio0"));
    assert_eq!(remote_mlines[mline_index].stream_ids, ["lk-stream"]);
    assert_eq!(
        remote_mlines[mline_index].track_id.as_deref(),
        Some("lk-track")
    );
    assert_eq!(remote_mlines[mline_index].ssrcs, [3503253273]);
    assert_eq!(
        super::ssrc_from_webrtc_stream_id("browser-generated-stream-id"),
        None
    );
}

#[test]
fn duplicate_ssrc_advertisements_do_not_choose_an_arbitrary_mline() {
    let remote_mlines = vec![
        super::RemoteMLineMetadata {
            stream_ids: vec!["first-stream".into()],
            track_id: Some("first-track".into()),
            mid: Some("audio0".into()),
            kind: Some(TransceiverKind::Audio),
            ssrcs: vec![7],
        },
        super::RemoteMLineMetadata {
            stream_ids: vec!["second-stream".into()],
            track_id: Some("second-track".into()),
            mid: Some("audio1".into()),
            kind: Some(TransceiverKind::Audio),
            ssrcs: vec![7],
        },
    ];

    let error = super::incoming_mline_for_rtp(&remote_mlines, Some(7), Some("audio1".into()), 1)
        .expect_err("an ambiguous SSRC mapping must be rejected");

    assert!(error.contains("advertised by more than one SDP m-line"));
}

#[test]
fn remote_answer_payload_is_applied_to_the_outgoing_rtp_branch() {
    gstreamer::init().expect("GStreamer must initialize for SDP payload tests");
    let remote_mline = super::MLineInfo {
        caps: super::codec_caps(TransceiverKind::Audio, 96),
        is_used: false,
        payload: 96,
    };
    let payload = super::negotiated_payload(&remote_mline, TransceiverKind::Audio)
        .expect("the answer must negotiate Opus on the audio m-line");
    let payloader = gstreamer::ElementFactory::make("rtpopuspay")
        .property("pt", 103u32)
        .build()
        .expect("Opus RTP payloader must be available");
    let capsfilter = gstreamer::ElementFactory::make("capsfilter")
        .property("caps", super::codec_caps(TransceiverKind::Audio, 103))
        .build()
        .expect("RTP capsfilter must be available");

    super::set_encoded_branch_payload(&payloader, &capsfilter, payload)
        .expect("the negotiated payload must be applied to the encoder branch");

    assert_eq!(payloader.property::<u32>("pt"), 96);
    let caps = capsfilter.property::<gstreamer::Caps>("caps");
    assert_eq!(
        caps.structure(0)
            .and_then(|structure| structure.get::<i32>("payload").ok()),
        Some(96)
    );
}

#[test]
fn outgoing_rtp_waits_for_answer_and_uses_its_payload_type() {
    gstreamer::init().expect("GStreamer must initialize for RTP payload tests");
    let expected_ssrc = 0x1020_3040;
    let pipeline = gstreamer::Pipeline::with_name("answer-gated outgoing RTP");
    let source = gstreamer::ElementFactory::make("audiotestsrc")
        .property("is-live", true)
        .build()
        .expect("audio test source must be available");
    let valve = gstreamer::ElementFactory::make("valve")
        .property("drop", true)
        .build()
        .expect("valve must be available");
    valve.set_property_from_str("drop-mode", "forward-sticky-events");
    let encoder = gstreamer::ElementFactory::make("opusenc")
        .build()
        .expect("Opus encoder must be available");
    let payloader = gstreamer::ElementFactory::make("rtpopuspay")
        .property("pt", 103u32)
        .property("ssrc", expected_ssrc)
        .build()
        .expect("Opus RTP payloader must be available");
    let capsfilter = gstreamer::ElementFactory::make("capsfilter")
        .property("caps", super::codec_caps(TransceiverKind::Audio, 103))
        .build()
        .expect("RTP capsfilter must be available");
    let sink = gstreamer::ElementFactory::make("appsink")
        .property("async", false)
        .property("sync", false)
        .build()
        .expect("appsink must be available")
        .downcast::<AppSink>()
        .expect("appsink must have the AppSink type");

    pipeline
        .add_many([
            &source,
            &valve,
            &encoder,
            &payloader,
            &capsfilter,
            sink.upcast_ref(),
        ])
        .expect("outgoing RTP elements must be added to the pipeline");
    gstreamer::Element::link_many([
        &source,
        &valve,
        &encoder,
        &payloader,
        &capsfilter,
        sink.upcast_ref(),
    ])
    .expect("outgoing RTP elements must link");
    pipeline
        .set_state(gstreamer::State::Playing)
        .expect("the outgoing RTP pipeline must start");

    assert!(
        sink.try_pull_sample(gstreamer::ClockTime::from_mseconds(100))
            .is_none(),
        "RTP must remain blocked until a remote answer selects its payload type"
    );

    super::reconfigure_encoded_branch_payload(&payloader, &valve, &capsfilter, 96)
        .expect("the negotiated payload must be applied before opening the branch");
    let sample = sink
        .try_pull_sample(gstreamer::ClockTime::from_seconds(3))
        .expect("the RTP branch must flow after the answer opens the valve");
    let buffer = sample.buffer().expect("RTP sample must have a buffer");
    let bytes = buffer.map_readable().expect("RTP buffer must be readable");
    assert!(bytes.as_slice().len() >= 2, "RTP header must be present");
    assert_eq!(
        bytes.as_slice()[1] & 0x7f,
        96,
        "the first RTP packet must carry the negotiated PT"
    );
    assert_eq!(
        u32::from_be_bytes(
            bytes.as_slice()[8..12]
                .try_into()
                .expect("RTP header must contain an SSRC"),
        ),
        expected_ssrc,
        "the RTP header SSRC must match the SSRC advertised in SDP"
    );

    pipeline
        .set_state(gstreamer::State::Null)
        .expect("the outgoing RTP pipeline must stop");
}

#[test]
fn two_gstreamer_peers_exchange_audio_and_reach_connected() {
    gstreamer::init().expect("GStreamer must initialize for the WebRTC loopback");

    let (events_sender, events_receiver) = channel();
    let offer_peer = Arc::new(Mutex::new(None));
    let answer_peer = Arc::new(Mutex::new(None));
    let offer = WebRtcController::new::<GStreamerBackend>(Box::new(LoopbackSignaller {
        name: "offerer",
        peer: offer_peer.clone(),
        events: events_sender.clone(),
    }));
    let answer = WebRtcController::new::<GStreamerBackend>(Box::new(LoopbackSignaller {
        name: "answerer",
        peer: answer_peer.clone(),
        events: events_sender,
    }));
    *offer_peer.lock().unwrap() = Some(answer.clone());
    *answer_peer.lock().unwrap() = Some(offer.clone());
    let peers = LoopbackPeers { offer, answer };

    let stream = GStreamerMediaStream::create_audio();
    peers.offer.add_stream(&stream);

    let offer_description = wait_for_description(&peers.offer, |controller, completion| {
        controller.create_offer(completion);
    });
    set_description(&peers.offer, offer_description.clone(), false);
    set_description(&peers.answer, offer_description, true);

    let answer_description = wait_for_description(&peers.answer, |controller, completion| {
        controller.create_answer(completion);
    });
    set_description(&peers.answer, answer_description.clone(), false);
    set_description(&peers.offer, answer_description, true);

    let mut offerer_connected = false;
    let mut answerer_connected = false;
    let mut incoming_audio = false;
    while !(offerer_connected && answerer_connected && incoming_audio) {
        match wait_for_event(&events_receiver) {
            Event::Connected("offerer") => offerer_connected = true,
            Event::Connected("answerer") => answerer_connected = true,
            Event::Failed(name) => panic!("{name} peer connection failed"),
            Event::IncomingAudio(_, _, _) => incoming_audio = true,
            Event::Connected(_) => unreachable!(),
        }
    }
}

#[test]
fn transceiver_api_sends_dom_msid_maps_tracks_and_delivers_incoming_pcm() {
    gstreamer::init().expect("GStreamer must initialize for the WebRTC loopback");

    let (events_sender, events_receiver) = channel();
    let offer_peer = Arc::new(Mutex::new(None));
    let answer_peer = Arc::new(Mutex::new(None));
    let offer = WebRtcController::new::<GStreamerBackend>(Box::new(LoopbackSignaller {
        name: "offerer",
        peer: offer_peer.clone(),
        events: events_sender.clone(),
    }));
    let answer = WebRtcController::new::<GStreamerBackend>(Box::new(LoopbackSignaller {
        name: "answerer",
        peer: answer_peer.clone(),
        events: events_sender,
    }));
    *offer_peer.lock().unwrap() = Some(answer.clone());
    *answer_peer.lock().unwrap() = Some(offer.clone());
    let peers = LoopbackPeers { offer, answer };

    peers.offer.configure("".into(), BundlePolicy::MaxBundle);
    peers.answer.configure("".into(), BundlePolicy::MaxBundle);

    for label in ["reliable", "lossy"] {
        let mut init = DataChannelInit::default();
        init.label = label.into();
        peers
            .offer
            .create_data_channel(init)
            .expect("webrtcbin should create each data channel");
    }

    let recvonly_audio: Vec<_> = (0..3)
        .map(|_| {
            peers
                .offer
                .add_transceiver(TransceiverKind::Audio, TransceiverDirection::RecvOnly, None)
                .expect("webrtcbin should add an audio receiver transceiver")
        })
        .collect();
    let recvonly_video: Vec<_> = (0..3)
        .map(|_| {
            peers
                .offer
                .add_transceiver(TransceiverKind::Video, TransceiverDirection::RecvOnly, None)
                .expect("webrtcbin should add a video receiver transceiver")
        })
        .collect();

    let states = peers
        .offer
        .get_transceivers()
        .expect("transceiver snapshots should be available");
    assert_eq!(states.len(), 6);
    for id in recvonly_audio.iter().chain(recvonly_video.iter()) {
        assert!(states.iter().any(|state| state.id == *id));
    }
    assert!(states.iter().any(|state| {
        state.id == recvonly_audio[0]
            && state.kind == TransceiverKind::Audio
            && state.direction == TransceiverDirection::RecvOnly
    }));
    peers
        .offer
        .set_transceiver_direction(recvonly_audio[0], TransceiverDirection::Inactive)
        .expect("the native transceiver direction should be mutable");
    assert!(
        peers
            .offer
            .get_transceivers()
            .expect("transceiver snapshots should be available")
            .iter()
            .any(|state| {
                state.id == recvonly_audio[0] && state.direction == TransceiverDirection::Inactive
            })
    );
    peers
        .offer
        .set_transceiver_direction(recvonly_audio[0], TransceiverDirection::RecvOnly)
        .expect("the native transceiver direction should be restorable");

    let first_offer = wait_for_description(&peers.offer, |controller, completion| {
        controller.create_offer(completion);
    });
    assert_eq!(
        first_offer.sdp.matches("\nm=").count(),
        7,
        "unexpected first offer SDP: {}",
        first_offer.sdp
    );
    assert_eq!(first_offer.sdp.matches("m=application").count(), 1);
    set_description(&peers.offer, first_offer.clone(), false);
    set_description(&peers.answer, first_offer, true);

    let first_answer = wait_for_description(&peers.answer, |controller, completion| {
        controller.create_answer(completion);
    });
    set_description(&peers.answer, first_answer.clone(), false);
    set_description(&peers.offer, first_answer, true);

    let track_source = GStreamerMediaStream::create_audio();
    let external_pipeline = gstreamer::Pipeline::with_name("already attached DOM track");
    let source = get_stream(&track_source).expect("the outgoing track should be registered");
    let mut source = source.lock().unwrap();
    source
        .as_mut_any()
        .downcast_mut::<GStreamerMediaStream>()
        .expect("the outgoing track should use the GStreamer backend")
        .attach_to_pipeline(&external_pipeline);
    drop(source);
    let sendonly_audio = peers
        .offer
        .add_transceiver(
            TransceiverKind::Audio,
            TransceiverDirection::SendOnly,
            Some(OutgoingTrack {
                source_stream: track_source,
                stream_ids: vec!["dom-stream-123".into(), "dom-stream-789".into()],
                track_id: "dom-track-456".into(),
                audio_bitrate: Some(32_000),
            }),
        )
        .expect("webrtcbin should add and connect a sendonly audio transceiver");
    assert!(
        peers
            .offer
            .get_transceivers()
            .expect("transceiver snapshots should be available")
            .iter()
            .any(|state| state.id == sendonly_audio)
    );
    let _streamless_audio = peers
        .offer
        .add_transceiver(
            TransceiverKind::Audio,
            TransceiverDirection::SendOnly,
            Some(OutgoingTrack {
                source_stream: GStreamerMediaStream::create_audio(),
                stream_ids: vec![],
                track_id: "dom-track-streamless".into(),
                audio_bitrate: None,
            }),
        )
        .expect("webrtcbin should add a streamless sendonly audio transceiver");

    let second_offer = wait_for_description(&peers.offer, |controller, completion| {
        controller.create_offer(completion);
    });
    assert_eq!(second_offer.sdp.matches("\nm=").count(), 9);
    assert_eq!(second_offer.sdp.matches("m=application").count(), 1);
    let mut payloads = std::collections::HashSet::new();
    for line in second_offer
        .sdp
        .lines()
        .filter(|line| line.starts_with("m=audio ") || line.starts_with("m=video "))
    {
        let payload = line
            .split_ascii_whitespace()
            .nth(3)
            .expect("RTP m-line should include a payload type");
        assert!(
            payloads.insert(payload),
            "BUNDLE m-lines must use distinct payload types: {}",
            second_offer.sdp
        );
    }
    assert!(
        second_offer
            .sdp
            .contains("a=msid:dom-stream-123 dom-track-456")
    );
    assert!(
        second_offer
            .sdp
            .contains("a=msid:dom-stream-789 dom-track-456")
    );
    assert!(second_offer.sdp.contains("a=msid:- dom-track-streamless"));
    let second_offer_sdp =
        super::gstreamer_sdp::SDPMessage::parse_buffer(second_offer.sdp.as_bytes())
            .expect("the second offer SDP must parse");
    let outgoing_audio = super::remote_mline_metadata(
        second_offer_sdp
            .media(7)
            .expect("the first outgoing audio m-line should exist"),
    )
    .expect("the first outgoing audio m-line metadata must parse");
    assert_eq!(
        outgoing_audio.stream_ids,
        ["dom-stream-123", "dom-stream-789"]
    );
    assert_eq!(outgoing_audio.track_id.as_deref(), Some("dom-track-456"));
    assert_eq!(outgoing_audio.ssrcs.len(), 1);
    let outgoing_streamless_audio = super::remote_mline_metadata(
        second_offer_sdp
            .media(8)
            .expect("the streamless outgoing audio m-line should exist"),
    )
    .expect("the streamless outgoing audio metadata must parse");
    assert!(outgoing_streamless_audio.stream_ids.is_empty());
    assert_eq!(
        outgoing_streamless_audio.track_id.as_deref(),
        Some("dom-track-streamless")
    );
    assert_eq!(outgoing_streamless_audio.ssrcs.len(), 1);
    assert_ne!(outgoing_audio.ssrcs, outgoing_streamless_audio.ssrcs);
    set_description(&peers.offer, second_offer.clone(), false);
    set_description(&peers.answer, second_offer.clone(), true);

    let second_answer = wait_for_description(&peers.answer, |controller, completion| {
        controller.create_answer(completion);
    });
    set_description(&peers.answer, second_answer.clone(), false);
    set_description(&peers.offer, second_answer, true);

    let mut observed_tracks = Vec::new();
    let mut received_tracks = std::collections::HashMap::new();
    let deadline = Instant::now() + TIMEOUT;
    while received_tracks.len() < 2 {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("WebRTC loopback did not deliver the published track");
        match events_receiver.recv_timeout(remaining) {
            Ok(Event::IncomingAudio(peer, stream_id, metadata)) => {
                if let Some(track_id) = metadata.track_id.as_ref() {
                    if track_id == "dom-track-456" || track_id == "dom-track-streamless" {
                        received_tracks.insert(track_id.clone(), (peer, stream_id, metadata));
                        continue;
                    }
                }
                observed_tracks.push((peer, metadata));
            },
            Ok(Event::Connected("offerer" | "answerer")) => {},
            Ok(Event::Failed(name)) => panic!("{name} peer connection failed"),
            Ok(Event::Connected(_)) => unreachable!(),
            Err(error) => panic!(
                "WebRTC loopback did not deliver the published track; observed incoming tracks: {observed_tracks:?}; {error}"
            ),
        }
    }

    let (incoming_peer, incoming_stream, metadata) = received_tracks
        .remove("dom-track-456")
        .expect("the receiver should get the track with DOM stream IDs");
    assert_eq!(incoming_peer, "answerer");
    assert_eq!(
        metadata.stream_ids,
        vec!["dom-stream-123", "dom-stream-789"],
        "incoming metadata: {metadata:?}; offer SDP:\n{}",
        second_offer.sdp
    );
    assert_eq!(metadata.track_id.as_deref(), Some("dom-track-456"));
    let incoming_audio = IncomingAudioConsumer::attach(incoming_stream);
    let pcm_rms = incoming_audio.receive_nonzero_pcm();
    assert!(
        pcm_rms > 0.01,
        "the transceiver receiver should deliver nonzero decoded PCM, RMS was {pcm_rms}"
    );
    let answer_states = peers
        .answer
        .get_transceivers()
        .expect("remote offer transceivers should be registered");
    assert!(
        answer_states
            .iter()
            .any(|state| { state.id == metadata.transceiver_id && state.mid == metadata.mid })
    );

    let (streamless_peer, _, streamless_metadata) = received_tracks
        .remove("dom-track-streamless")
        .expect("the receiver should get the streamless track");
    assert_eq!(streamless_peer, "answerer");
    assert!(streamless_metadata.stream_ids.is_empty());
    assert_eq!(
        streamless_metadata.track_id.as_deref(),
        Some("dom-track-streamless")
    );
    assert!(
        peers
            .answer
            .get_transceivers()
            .expect("remote offer transceivers should be registered")
            .iter()
            .any(|state| {
                state.id == streamless_metadata.transceiver_id
                    && state.mid == streamless_metadata.mid
            })
    );
}

#[test]
fn ice_configuration_accepts_initial_turn_and_rejects_removal() {
    gstreamer::init().expect("GStreamer must initialize for the WebRTC loopback");
    let (events_sender, _) = channel();
    let controller = WebRtcController::new::<GStreamerBackend>(Box::new(LoopbackSignaller {
        name: "ice-config",
        peer: Arc::new(Mutex::new(None)),
        events: events_sender,
    }));
    controller
        .set_ice_configuration(IceConfiguration {
            stun_server: None,
            turn_servers: Vec::new(),
            policy: IceTransportPolicy::All,
        })
        .expect("an empty initial ICE configuration should apply");
    controller
        .set_ice_configuration(IceConfiguration {
            stun_server: None,
            turn_servers: vec!["turn://user:password@127.0.0.1:3478".into()],
            policy: IceTransportPolicy::Relay,
        })
        .expect("an initial TURN server should be addable");
    assert!(
        controller
            .set_ice_configuration(IceConfiguration {
                stun_server: None,
                turn_servers: Vec::new(),
                policy: IceTransportPolicy::All,
            })
            .is_err()
    );
    controller.quit();
}
