/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::{cmp, mem};

use glib;
use glib::prelude::*;
use gstreamer;
use gstreamer::prelude::*;
use gstreamer_sdp;
use gstreamer_webrtc;
use log::warn;
use servo_media_streams::MediaStreamType;
use servo_media_streams::registry::{MediaStreamId, get_stream, stop_stream, unregister_stream};
use servo_media_webrtc::datachannel::DataChannelId;
use servo_media_webrtc::thread::InternalEvent;
use servo_media_webrtc::{
    OutgoingTrack, TransceiverDirection, TransceiverId, TransceiverKind, TransceiverState,
    WebRtcController as WebRtcThread, *,
};

use super::BACKEND_BASE_TIME;
use crate::datachannel::GStreamerWebRtcDataChannel;
use crate::media_stream::GStreamerMediaStream;

#[cfg(test)]
#[path = "webrtc_tests.rs"]
mod tests;

// TODO:
// - figure out purpose of glib loop

#[derive(Debug, Clone)]
pub struct MLineInfo {
    /// The caps for the given m-line
    caps: gstreamer::Caps,
    /// Whether or not this sink pad has already been connected
    is_used: bool,
    /// The payload value of the given m-line
    payload: i32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RemoteMLineMetadata {
    stream_ids: Vec<String>,
    track_id: Option<String>,
    mid: Option<String>,
    kind: Option<TransceiverKind>,
    ssrcs: Vec<u32>,
}

#[derive(Default)]
struct TransceiverRegistry {
    next_id: u64,
    by_id: HashMap<TransceiverId, RegisteredTransceiver>,
    by_native_pointer: HashMap<usize, TransceiverId>,
}

struct RegisteredTransceiver {
    native: gstreamer_webrtc::WebRTCRTPTransceiver,
    kind: TransceiverKind,
    payload: Option<i32>,
    outgoing_msid: Option<OutgoingMsid>,
    outgoing_ssrc: Option<u32>,
    outgoing_payloader: Option<gstreamer::Element>,
    outgoing_valve: Option<gstreamer::Element>,
    outgoing_capsfilter: Option<gstreamer::Element>,
}

#[derive(Clone)]
struct OutgoingMsid {
    stream_ids: Vec<String>,
    track_id: String,
}

impl TransceiverRegistry {
    fn register(
        &mut self,
        native: gstreamer_webrtc::WebRTCRTPTransceiver,
        kind: TransceiverKind,
        payload: Option<i32>,
    ) -> Result<TransceiverId, WebRtcError> {
        let pointer = native.as_ptr() as usize;
        if let Some(id) = self.by_native_pointer.get(&pointer).copied() {
            if let Some(entry) = self.by_id.get_mut(&id) {
                entry.kind = kind;
                if payload.is_some() {
                    entry.payload = payload;
                }
                return Ok(id);
            }
        }

        let id = TransceiverId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| WebRtcError::Backend("transceiver ID space exhausted".to_owned()))?;
        self.by_native_pointer.insert(pointer, id);
        self.by_id.insert(
            id,
            RegisteredTransceiver {
                native,
                kind,
                payload,
                outgoing_msid: None,
                outgoing_ssrc: None,
                outgoing_payloader: None,
                outgoing_valve: None,
                outgoing_capsfilter: None,
            },
        );
        Ok(id)
    }

    fn id_for(&self, native: &gstreamer_webrtc::WebRTCRTPTransceiver) -> Option<TransceiverId> {
        self.by_native_pointer
            .get(&(native.as_ptr() as usize))
            .copied()
    }

    fn id_for_mid(&self, mid: &str) -> Option<TransceiverId> {
        self.by_id.iter().find_map(|(id, entry)| {
            (entry
                .native
                .property::<Option<glib::GString>>("mid")
                .as_deref()
                == Some(mid))
            .then_some(*id)
        })
    }

    fn state(
        id: TransceiverId,
        entry: &RegisteredTransceiver,
    ) -> Result<TransceiverState, WebRtcError> {
        let direction = match entry.native.direction() {
            gstreamer_webrtc::WebRTCRTPTransceiverDirection::Sendrecv => {
                TransceiverDirection::SendRecv
            },
            gstreamer_webrtc::WebRTCRTPTransceiverDirection::Sendonly => {
                TransceiverDirection::SendOnly
            },
            gstreamer_webrtc::WebRTCRTPTransceiverDirection::Recvonly => {
                TransceiverDirection::RecvOnly
            },
            gstreamer_webrtc::WebRTCRTPTransceiverDirection::Inactive => {
                TransceiverDirection::Inactive
            },
            direction => {
                return Err(WebRtcError::Backend(format!(
                    "unknown native transceiver direction: {direction:?}"
                )));
            },
        };
        let native_mline_index = entry.native.mlineindex();
        Ok(TransceiverState {
            id,
            kind: entry.kind,
            direction,
            mid: entry
                .native
                .property::<Option<glib::GString>>("mid")
                .map(Into::into),
            mline_index: (native_mline_index != u32::MAX)
                .then(|| u16::try_from(native_mline_index).ok())
                .flatten(),
        })
    }

    fn snapshots(&self) -> Vec<TransceiverState> {
        let mut entries: Vec<_> = self.by_id.iter().collect();
        entries.sort_by_key(|(id, _)| id.0);
        entries
            .into_iter()
            .filter_map(|(id, entry)| match Self::state(*id, entry) {
                Ok(state) => Some(state),
                Err(error) => {
                    warn!("Could not read transceiver state: {error:?}");
                    None
                },
            })
            .collect()
    }
}

enum DataChannelEventTarget {
    Buffered(Vec<DataChannelEvent>),
    Created(GStreamerWebRtcDataChannel),
}

pub struct GStreamerWebRtcController {
    webrtc: gstreamer::Element,
    pipeline: gstreamer::Pipeline,
    /// We can't trigger a negotiation-needed event until we have streams, or otherwise
    /// a createOffer() call will lead to bad SDP. Instead, we delay negotiation.
    delayed_negotiation: bool,
    /// A handle to the event loop abstraction surrounding the webrtc implementations,
    /// which lets gstreamer callbacks send events back to the event loop to run on this object
    thread: WebRtcThread,
    signaller: Box<dyn WebRtcSignaller>,
    /// All the streams that are actually connected to the webrtcbin (i.e., their presence has already
    /// been negotiated)
    streams: Vec<MediaStreamId>,
    /// Branches created when a DOM track already belongs to another pipeline.
    internal_streams: Vec<MediaStreamId>,
    /// Disconnected streams that are waiting to be linked. Streams are
    /// only linked when:
    ///
    /// - An offer is made (all pending streams are flushed)
    /// - An offer is received (all matching pending streams are flushed)
    /// - A stream is added when there is a so-far-disconnected remote-m-line
    ///
    /// In other words, these are all yet to be negotiated
    ///
    /// See link_stream
    pending_streams: Vec<MediaStreamId>,
    /// Each new webrtc stream should have a new payload/pt value, starting at 96
    ///
    /// This is maintained as a known yet-unused payload number, being incremented whenever
    /// we use it, and set to (remote_pt + 1) if the remote sends us a stream with a higher pt
    pt_counter: i32,
    /// We keep track of how many request pads have been created on webrtcbin
    /// so that we can request more to fill in the gaps and acquire a specific pad if necessary
    request_pad_counter: usize,
    /// Streams need to be connected to the relevant sink pad, and we figure this out
    /// by keeping track of the caps of each m-line in the SDP.
    remote_mline_info: Vec<MLineInfo>,
    remote_mline_metadata: Arc<Mutex<Vec<RemoteMLineMetadata>>>,
    transceivers: Arc<Mutex<TransceiverRegistry>>,
    /// Servers already committed to webrtcbin, which has no removal action.
    turn_servers: Vec<String>,
    /// Temporary storage for remote_mline_info until the remote description is applied
    ///
    /// Without this, a unluckily timed call to link_stream() may happen before the webrtcbin
    /// knows the remote description, but while we _think_ it does
    pending_remote_mline_info: Vec<MLineInfo>,
    pending_remote_mline_metadata: Vec<RemoteMLineMetadata>,
    local_mline_count: usize,
    /// In case we get multiple remote offers, this lets us keep track of which is the newest
    remote_description_generation: u32,
    _main_loop: glib::MainLoop,
    data_channels: Arc<Mutex<HashMap<DataChannelId, DataChannelEventTarget>>>,
    next_data_channel_id: Arc<AtomicUsize>,
}

impl WebRtcControllerBackend for GStreamerWebRtcController {
    fn add_ice_candidate(&mut self, candidate: IceCandidate) -> WebRtcResult {
        self.webrtc.emit_by_name::<()>(
            "add-ice-candidate",
            &[&candidate.sdp_mline_index, &candidate.candidate],
        );
        Ok(())
    }

    fn set_remote_description(
        &mut self,
        desc: SessionDescription,
        cb: Box<dyn FnOnce() + Send + 'static>,
    ) -> WebRtcResult {
        self.set_description(desc, DescriptionType::Remote, cb)
    }

    fn set_local_description(
        &mut self,
        desc: SessionDescription,
        cb: Box<dyn FnOnce() + Send + 'static>,
    ) -> WebRtcResult {
        self.set_description(desc, DescriptionType::Local, cb)
    }

    fn create_offer(
        &mut self,
        cb: Box<dyn FnOnce(SessionDescription) + Send + 'static>,
    ) -> WebRtcResult {
        self.flush_pending_streams(true)?;
        self.pipeline.set_state(gstreamer::State::Playing)?;
        let transceivers = Arc::clone(&self.transceivers);
        let promise = gstreamer::Promise::with_change_func(move |res| {
            res.map(|s| {
                on_offer_or_answer_created(SdpType::Offer, s.unwrap(), cb, Some(transceivers))
            })
            .unwrap();
        });

        self.webrtc
            .emit_by_name::<()>("create-offer", &[&None::<gstreamer::Structure>, &promise]);
        Ok(())
    }

    fn create_answer(
        &mut self,
        cb: Box<dyn FnOnce(SessionDescription) + Send + 'static>,
    ) -> WebRtcResult {
        let transceivers = Arc::clone(&self.transceivers);
        let promise = gstreamer::Promise::with_change_func(move |res| {
            res.map(|s| {
                on_offer_or_answer_created(SdpType::Answer, s.unwrap(), cb, Some(transceivers))
            })
            .unwrap();
        });

        self.webrtc
            .emit_by_name::<()>("create-answer", &[&None::<gstreamer::Structure>, &promise]);
        Ok(())
    }

    fn add_stream(&mut self, stream_id: &MediaStreamId) -> WebRtcResult {
        let stream =
            get_stream(stream_id).expect("Media streams registry does not contain such ID");
        let mut stream = stream.lock().unwrap();
        let stream = stream
            .as_mut_any()
            .downcast_mut::<GStreamerMediaStream>()
            .ok_or("Does not currently support non-gstreamer streams")?;
        self.link_stream(stream_id, stream, false)?;
        if self.delayed_negotiation && (self.streams.len() > 1 || self.pending_streams.len() > 1) {
            self.delayed_negotiation = false;
            self.signaller.on_negotiation_needed(&self.thread);
        }
        Ok(())
    }

    fn add_transceiver(
        &mut self,
        kind: TransceiverKind,
        direction: TransceiverDirection,
        track: Option<OutgoingTrack>,
    ) -> Result<TransceiverId, WebRtcError> {
        if let Some(track) = &track {
            if track.audio_bitrate.is_some() && kind != TransceiverKind::Audio {
                return Err(WebRtcError::Backend(
                    "audio bitrate is only supported for audio tracks".into(),
                ));
            }
            if track
                .audio_bitrate
                .is_some_and(|bitrate| !(4_000..=650_000).contains(&bitrate))
            {
                return Err(WebRtcError::Backend(
                    "Opus bitrate must be between 4000 and 650000 bits per second".into(),
                ));
            }
            let source = get_stream(&track.source_stream).ok_or_else(|| {
                WebRtcError::Backend("outgoing media stream does not exist".into())
            })?;
            let mut source = source.lock().unwrap();
            let source = source
                .as_mut_any()
                .downcast_mut::<GStreamerMediaStream>()
                .ok_or_else(|| {
                    WebRtcError::Backend("outgoing media stream is not backed by GStreamer".into())
                })?;
            if !source.caps().can_intersect(&codec_caps(kind, 96)) {
                return Err(WebRtcError::Backend(
                    "outgoing media stream kind does not match the transceiver".into(),
                ));
            }
        }

        let payload = self.allocate_payload()?;
        let codec_caps = codec_caps(kind, payload);
        let transceiver = if track.is_some() {
            self.request_track_transceiver(kind, direction, payload)?
        } else {
            self.webrtc
                .emit_by_name::<Option<gstreamer_webrtc::WebRTCRTPTransceiver>>(
                    "add-transceiver",
                    &[&native_direction(direction), &codec_caps],
                )
                .ok_or_else(|| {
                    WebRtcError::Backend("webrtcbin could not add a transceiver".into())
                })?
        };
        let id =
            self.transceivers
                .lock()
                .unwrap()
                .register(transceiver.clone(), kind, Some(payload))?;

        if let Some(track) = track {
            if let Err(error) = self.link_track_to_transceiver(id, &transceiver, &track) {
                // A requested sink pad owns its native transceiver and webrtcbin
                // has no public action for removing it. Keep the failed m-line
                // inactive so it cannot accidentally send media.
                transceiver
                    .set_direction(gstreamer_webrtc::WebRTCRTPTransceiverDirection::Inactive);
                return Err(error);
            }
            if let Some(entry) = self.transceivers.lock().unwrap().by_id.get_mut(&id) {
                entry.outgoing_msid = Some(OutgoingMsid {
                    stream_ids: track.stream_ids,
                    track_id: track.track_id,
                });
            }
        }
        Ok(id)
    }

    fn set_transceiver_direction(
        &mut self,
        id: TransceiverId,
        direction: TransceiverDirection,
    ) -> WebRtcResult {
        let transceivers = self.transceivers.lock().unwrap();
        let transceiver = transceivers
            .by_id
            .get(&id)
            .ok_or_else(|| WebRtcError::Backend("unknown transceiver ID".into()))?;
        transceiver
            .native
            .set_direction(native_direction(direction));
        Ok(())
    }

    fn get_transceivers(&mut self) -> Vec<TransceiverState> {
        if let Err(error) = self.refresh_transceivers() {
            warn!("Could not refresh native transceivers: {error:?}");
        }
        self.transceivers.lock().unwrap().snapshots()
    }

    fn create_data_channel(&mut self, init: &DataChannelInit) -> WebRtcDataChannelResult {
        let id = self.next_data_channel_id.fetch_add(1, Ordering::Relaxed);
        match GStreamerWebRtcDataChannel::new(&id, &self.webrtc, &self.thread, init) {
            Ok(channel) => register_data_channel(self.data_channels.clone(), id, channel),
            Err(error) => Err(WebRtcError::Backend(error)),
        }
    }

    fn close_data_channel(&mut self, id: &DataChannelId) -> WebRtcResult {
        // There is no need to unregister the channel here. It will be unregistered
        // when the data channel backend triggers the on closed event.
        let mut data_channels = self.data_channels.lock().unwrap();
        match data_channels.get(id) {
            Some(ref channel) => match channel {
                DataChannelEventTarget::Created(channel) => {
                    channel.close();
                    Ok(())
                },
                DataChannelEventTarget::Buffered(_) => data_channels
                    .remove(id)
                    .ok_or(WebRtcError::Backend("Unknown data channel".to_owned()))
                    .map(|_| ()),
            },
            None => Err(WebRtcError::Backend("Unknown data channel".to_owned())),
        }
    }

    fn send_data_channel_message(
        &mut self,
        id: &DataChannelId,
        message: &DataChannelMessage,
    ) -> WebRtcResult {
        match self.data_channels.lock().unwrap().get(id) {
            Some(ref channel) => match channel {
                DataChannelEventTarget::Created(channel) => {
                    channel.send(message);
                    Ok(())
                },
                _ => Ok(()),
            },
            None => Err(WebRtcError::Backend("Unknown data channel".to_owned())),
        }
    }

    fn configure(&mut self, stun_server: &str, policy: BundlePolicy) -> WebRtcResult {
        if !stun_server.is_empty() {
            self.webrtc
                .set_property_from_str("stun-server", stun_server);
        }
        self.webrtc
            .set_property_from_str("bundle-policy", policy.as_str());
        Ok(())
    }

    fn set_ice_configuration(&mut self, config: IceConfiguration) -> WebRtcResult {
        if !config.turn_servers.starts_with(&self.turn_servers) {
            return Err(WebRtcError::Backend(
                "webrtcbin cannot remove or reorder an applied TURN server".into(),
            ));
        }

        for server in config.turn_servers.iter().skip(self.turn_servers.len()) {
            let added = self
                .webrtc
                .emit_by_name::<bool>("add-turn-server", &[server]);
            if !added {
                return Err(WebRtcError::Backend(
                    "webrtcbin could not add a configured TURN server".into(),
                ));
            }
            self.turn_servers.push(server.clone());
        }

        self.webrtc
            .set_property("stun-server", config.stun_server.as_deref());
        let policy = match config.policy {
            IceTransportPolicy::All => gstreamer_webrtc::WebRTCICETransportPolicy::All,
            IceTransportPolicy::Relay => gstreamer_webrtc::WebRTCICETransportPolicy::Relay,
        };
        self.webrtc.set_property("ice-transport-policy", policy);
        Ok(())
    }

    fn internal_event(&mut self, e: thread::InternalEvent) -> WebRtcResult {
        match e {
            InternalEvent::UpdateConnectionState => {
                let state = self
                    .webrtc
                    .property::<gstreamer_webrtc::WebRTCPeerConnectionState>("connection-state");
                self.signaller
                    .update_connection_state(connection_state(state)?);
            },
            InternalEvent::OnNegotiationNeeded => {
                if self.streams.is_empty()
                    && self.pending_streams.is_empty()
                    && self.transceivers.lock().unwrap().by_id.is_empty()
                {
                    // we have no streams

                    // If the pipeline starts playing and on-negotiation-needed is present before there are any
                    // media streams, an invalid SDP offer will be created. Therefore, delay emitting the signal
                    self.delayed_negotiation = true;
                } else {
                    self.signaller.on_negotiation_needed(&self.thread);
                }
            },
            InternalEvent::OnIceCandidate(candidate) => {
                self.signaller.on_ice_candidate(&self.thread, candidate);
            },
            InternalEvent::OnAddStream(stream, ty, metadata) => {
                self.pipeline.set_state(gstreamer::State::Playing)?;
                self.signaller.on_add_stream(&stream, ty, metadata);
            },
            InternalEvent::OnDataChannelEvent(channel_id, event) => {
                let mut data_channels = self.data_channels.lock().unwrap();
                match data_channels.get_mut(&channel_id) {
                    None => {
                        data_channels
                            .insert(channel_id, DataChannelEventTarget::Buffered(vec![event]));
                    },
                    Some(ref mut channel) => match channel {
                        &mut &mut DataChannelEventTarget::Buffered(ref mut events) => {
                            events.push(event);
                            return Ok(());
                        },
                        DataChannelEventTarget::Created(_) => {
                            if let DataChannelEvent::Close = event {
                                data_channels.remove(&channel_id);
                            }
                            self.signaller
                                .on_data_channel_event(channel_id, event, &self.thread);
                        },
                    },
                }
            },
            InternalEvent::DescriptionAdded(
                cb,
                description_type,
                ty,
                remote_description_generation,
            ) => {
                if commit_remote_mline_info(
                    description_type,
                    ty,
                    remote_description_generation,
                    self.remote_description_generation,
                    &mut self.pending_remote_mline_info,
                    &mut self.remote_mline_info,
                    &mut self.pending_remote_mline_metadata,
                    &self.remote_mline_metadata,
                ) {
                    self.refresh_transceivers()?;
                    if description_type == DescriptionType::Remote && ty == SdpType::Answer {
                        self.apply_remote_answer_payloads()?;
                    }
                    self.flush_pending_streams(false)?;
                }
                if description_type == DescriptionType::Local && ty == SdpType::Answer {
                    self.apply_remote_answer_payloads()?;
                }
                self.pipeline.set_state(gstreamer::State::Playing)?;
                cb();
            },
            InternalEvent::UpdateSignalingState => {
                use gstreamer_webrtc::WebRTCSignalingState::*;
                let val = self
                    .webrtc
                    .property::<gstreamer_webrtc::WebRTCSignalingState>("signaling-state");
                let state = match val {
                    Stable => SignalingState::Stable,
                    HaveLocalOffer => SignalingState::HaveLocalOffer,
                    HaveRemoteOffer => SignalingState::HaveRemoteOffer,
                    HaveLocalPranswer => SignalingState::HaveLocalPranswer,
                    HaveRemotePranswer => SignalingState::HaveRemotePranswer,
                    Closed => SignalingState::Closed,
                    i => {
                        return Err(WebRtcError::Backend(format!(
                            "unknown signaling state: {:?}",
                            i
                        )));
                    },
                };
                self.signaller.update_signaling_state(state);
            },
            InternalEvent::UpdateGatheringState => {
                use gstreamer_webrtc::WebRTCICEGatheringState::*;
                let val = self
                    .webrtc
                    .property::<gstreamer_webrtc::WebRTCICEGatheringState>("ice-gathering-state");
                let state = match val {
                    New => GatheringState::New,
                    Gathering => GatheringState::Gathering,
                    Complete => GatheringState::Complete,
                    i => {
                        return Err(WebRtcError::Backend(format!(
                            "unknown gathering state: {:?}",
                            i
                        )));
                    },
                };
                self.signaller.update_gathering_state(state);
            },
            InternalEvent::UpdateIceConnectionState => {
                use gstreamer_webrtc::WebRTCICEConnectionState::*;
                let val = self
                    .webrtc
                    .property::<gstreamer_webrtc::WebRTCICEConnectionState>("ice-connection-state");
                let state = match val {
                    New => IceConnectionState::New,
                    Checking => IceConnectionState::Checking,
                    Connected => IceConnectionState::Connected,
                    Completed => IceConnectionState::Completed,
                    Disconnected => IceConnectionState::Disconnected,
                    Failed => IceConnectionState::Failed,
                    Closed => IceConnectionState::Closed,
                    i => {
                        return Err(WebRtcError::Backend(format!(
                            "unknown ICE connection state: {:?}",
                            i
                        )));
                    },
                };
                self.signaller.update_ice_connection_state(state);
            },
        }
        Ok(())
    }

    fn quit(&mut self) {
        self.signaller.close();

        self.pipeline.set_state(gstreamer::State::Null).unwrap();
        self.cleanup_internal_streams();
    }
}

fn commit_remote_mline_info(
    description_type: DescriptionType,
    ty: SdpType,
    description_generation: u32,
    current_generation: u32,
    pending_info: &mut Vec<MLineInfo>,
    current_info: &mut Vec<MLineInfo>,
    pending_metadata: &mut Vec<RemoteMLineMetadata>,
    current_metadata: &Mutex<Vec<RemoteMLineMetadata>>,
) -> bool {
    if description_type != DescriptionType::Remote
        || !matches!(ty, SdpType::Offer | SdpType::Answer)
        || description_generation != current_generation
    {
        return false;
    }

    mem::swap(pending_info, current_info);
    *current_metadata.lock().unwrap() = mem::take(pending_metadata);
    pending_info.clear();
    true
}

impl GStreamerWebRtcController {
    fn cleanup_internal_streams(&mut self) {
        for stream in self.internal_streams.drain(..) {
            if let Err(error) = stop_stream(&stream) {
                warn!("Could not stop an internal WebRTC media branch: {error}");
            }
            unregister_stream(&stream);
        }
    }

    fn allocate_payload(&mut self) -> Result<i32, WebRtcError> {
        let payload = self.pt_counter;
        if !(96..=127).contains(&payload) {
            return Err(WebRtcError::Backend(
                "no unused dynamic RTP payload type is available for another bundled transceiver"
                    .into(),
            ));
        }
        self.pt_counter += 1;
        Ok(payload)
    }

    fn allocate_outgoing_ssrc(&self) -> u32 {
        loop {
            let ssrc = rand::random::<u32>();
            if ssrc != 0
                && ssrc != u32::MAX
                && !self
                    .transceivers
                    .lock()
                    .unwrap()
                    .by_id
                    .values()
                    .any(|entry| entry.outgoing_ssrc == Some(ssrc))
            {
                return ssrc;
            }
        }
    }

    fn native_transceiver_count(&self) -> Result<u32, WebRtcError> {
        let mut count = 0u32;
        loop {
            let index = i32::try_from(count)
                .map_err(|_| WebRtcError::Backend("too many native transceivers".into()))?;
            let native = self
                .webrtc
                .emit_by_name::<Option<gstreamer_webrtc::WebRTCRTPTransceiver>>(
                    "get-transceiver",
                    &[&index],
                );
            if native.is_none() {
                return Ok(count);
            }
            count = count
                .checked_add(1)
                .ok_or_else(|| WebRtcError::Backend("too many native transceivers".into()))?;
        }
    }

    fn request_track_transceiver(
        &self,
        kind: TransceiverKind,
        direction: TransceiverDirection,
        payload: i32,
    ) -> Result<gstreamer_webrtc::WebRTCRTPTransceiver, WebRtcError> {
        let native_count = self.native_transceiver_count()?;
        let mline_index = self.unassigned_transceiver_mline_index(native_count)?;
        let name = format!("sink_{mline_index}");
        let pad = match self.webrtc.static_pad(&name) {
            Some(pad) => pad,
            None => self
                .webrtc
                .request_pad_simple(&name)
                .ok_or_else(|| WebRtcError::Backend(format!("could not request {name}")))?,
        };
        if pad.is_linked() {
            return Err(WebRtcError::Backend(format!(
                "{name} is already linked to another outgoing track"
            )));
        }
        let transceiver = pad
            .property::<Option<gstreamer_webrtc::WebRTCRTPTransceiver>>("transceiver")
            .ok_or_else(|| WebRtcError::Backend(format!("{name} has no transceiver")))?;
        if transceiver.mlineindex() != mline_index {
            return Err(WebRtcError::Backend(format!(
                "{name} is associated with native m-line {} instead of requested m-line {mline_index}",
                transceiver.mlineindex(),
            )));
        }
        if transceiver.find_property("codec-preferences").is_none() {
            return Err(WebRtcError::Backend(
                "GStreamer transceivers do not support codec preferences".into(),
            ));
        }
        transceiver.set_property("codec-preferences", Some(codec_caps(kind, payload)));
        transceiver.set_direction(native_direction(direction));
        Ok(transceiver)
    }

    fn refresh_transceivers(&mut self) -> WebRtcResult {
        let remote_metadata = self.remote_mline_metadata.lock().unwrap().clone();
        let registered_count = self.transceivers.lock().unwrap().by_id.len();
        let remote_transceiver_count = remote_metadata
            .iter()
            .filter(|metadata| metadata.kind.is_some())
            .count();
        let transceiver_count = registered_count.max(remote_transceiver_count);

        for index in 0..transceiver_count {
            let index = i32::try_from(index)
                .map_err(|_| WebRtcError::Backend("too many native transceivers".into()))?;
            let native = self
                .webrtc
                .emit_by_name::<Option<gstreamer_webrtc::WebRTCRTPTransceiver>>(
                    "get-transceiver",
                    &[&index],
                );
            let Some(native) = native else {
                return Err(WebRtcError::Backend(format!(
                    "webrtcbin has no transceiver at native index {index}"
                )));
            };
            let existing_id = self.transceivers.lock().unwrap().id_for(&native);
            if existing_id.is_some() {
                continue;
            }

            let kind = transceiver_remote_kind(&native, &remote_metadata);
            let Some(kind) = kind else {
                continue;
            };
            self.transceivers
                .lock()
                .unwrap()
                .register(native, kind, None)?;
        }
        Ok(())
    }

    fn native_transceiver_index(
        &self,
        target: &gstreamer_webrtc::WebRTCRTPTransceiver,
    ) -> Result<u32, WebRtcError> {
        let registered_count = self.transceivers.lock().unwrap().by_id.len();
        let remote_count = self
            .remote_mline_metadata
            .lock()
            .unwrap()
            .iter()
            .filter(|metadata| metadata.kind.is_some())
            .count();
        for index in 0..registered_count.max(remote_count) {
            let index = i32::try_from(index)
                .map_err(|_| WebRtcError::Backend("too many native transceivers".into()))?;
            let native = self
                .webrtc
                .emit_by_name::<Option<gstreamer_webrtc::WebRTCRTPTransceiver>>(
                    "get-transceiver",
                    &[&index],
                );
            let Some(native) = native else {
                break;
            };
            if native.as_ptr() == target.as_ptr() {
                return u32::try_from(index)
                    .map_err(|_| WebRtcError::Backend("invalid transceiver index".into()));
            }
        }
        Err(WebRtcError::Backend(
            "native transceiver is not present in webrtcbin".into(),
        ))
    }

    fn sink_pad_for_transceiver(
        &self,
        native: &gstreamer_webrtc::WebRTCRTPTransceiver,
    ) -> Result<(gstreamer::Pad, u32), WebRtcError> {
        let transceiver_index = self.native_transceiver_index(native)?;
        // Once assigned, webrtcbin sink pad names follow SDP m-line order.
        // Before negotiation the native transceiver has no m-line, so use
        // its position in webrtcbin's transceiver array.
        let mline_index = native.mlineindex();
        let pad_index = if mline_index == u32::MAX {
            self.unassigned_transceiver_mline_index(transceiver_index)?
        } else {
            mline_index
        };
        let name = format!("sink_{pad_index}");
        let pad = match self.webrtc.static_pad(&name) {
            Some(pad) => pad,
            None => self
                .webrtc
                .request_pad_simple(&name)
                .ok_or_else(|| WebRtcError::Backend(format!("could not request {name}")))?,
        };
        let pad_transceiver = pad
            .property::<Option<gstreamer_webrtc::WebRTCRTPTransceiver>>("transceiver")
            .ok_or_else(|| WebRtcError::Backend(format!("{name} has no transceiver")))?;
        if pad_transceiver.as_ptr() != native.as_ptr() {
            return Err(WebRtcError::Backend(format!(
                "{name} is associated with native m-line {} instead of requested m-line {} (transceiver ordinal {})",
                pad_transceiver.mlineindex(),
                native.mlineindex(),
                transceiver_index,
            )));
        }
        Ok((pad, pad_index))
    }

    fn unassigned_transceiver_mline_index(
        &self,
        transceiver_index: u32,
    ) -> Result<u32, WebRtcError> {
        let remote_mline_count = self.remote_mline_metadata.lock().unwrap().len();
        let mut next_mline_index = u32::try_from(self.local_mline_count.max(remote_mline_count))
            .map_err(|_| WebRtcError::Backend("too many SDP m-lines".into()))?;
        for entry in self.transceivers.lock().unwrap().by_id.values() {
            let index = entry.native.mlineindex();
            if index != u32::MAX {
                next_mline_index = next_mline_index.max(index.saturating_add(1));
            }
        }

        let mut unassigned_before = 0u32;
        for index in 0..transceiver_index {
            let index = i32::try_from(index)
                .map_err(|_| WebRtcError::Backend("too many native transceivers".into()))?;
            let native = self
                .webrtc
                .emit_by_name::<Option<gstreamer_webrtc::WebRTCRTPTransceiver>>(
                    "get-transceiver",
                    &[&index],
                )
                .ok_or_else(|| {
                    WebRtcError::Backend(format!(
                        "webrtcbin has no transceiver at native index {index}"
                    ))
                })?;
            if native.mlineindex() == u32::MAX {
                unassigned_before = unassigned_before.saturating_add(1);
            }
        }
        next_mline_index
            .checked_add(unassigned_before)
            .ok_or_else(|| WebRtcError::Backend("SDP m-line index overflow".into()))
    }

    fn link_track_to_transceiver(
        &mut self,
        transceiver_id: TransceiverId,
        native: &gstreamer_webrtc::WebRTCRTPTransceiver,
        track: &OutgoingTrack,
    ) -> WebRtcResult {
        let (stream_id, is_internal_branch) =
            stream_for_pipeline(&track.source_stream, &self.pipeline)?;
        let result = (|| {
            let stream = get_stream(&stream_id).ok_or_else(|| {
                WebRtcError::Backend("outgoing media stream does not exist".into())
            })?;
            let mut stream = stream.lock().unwrap();
            let source = stream
                .as_mut_any()
                .downcast_mut::<GStreamerMediaStream>()
                .ok_or_else(|| {
                    WebRtcError::Backend("outgoing media stream is not backed by GStreamer".into())
                })?;
            let (expected_kind, payload) = self
                .transceivers
                .lock()
                .unwrap()
                .by_id
                .get(&transceiver_id)
                .and_then(|entry| entry.payload.map(|payload| (entry.kind, payload)))
                .ok_or_else(|| WebRtcError::Backend("unknown transceiver ID".into()))?;
            if !source
                .caps()
                .can_intersect(&codec_caps(expected_kind, payload))
            {
                return Err(WebRtcError::Backend(
                    "outgoing media stream kind does not match the transceiver".into(),
                ));
            }

            // Build the encoder branch before starting it. Publishing may happen
            // after the initial negotiation, when the pipeline is already playing.
            let outgoing_ssrc = self.allocate_outgoing_ssrc();
            let (element, payloader, valve, branch) = encoded_track_branch(
                source,
                expected_kind,
                payload,
                outgoing_ssrc,
                track.audio_bitrate,
            )?;
            let mut caps = source.caps_with_payload(payload);
            let msid = track
                .stream_ids
                .first()
                .map(|stream_id| format!("{stream_id} {}", track.track_id))
                .unwrap_or_else(|| format!("- {}", track.track_id));
            if let Some(structure) = caps.get_mut().and_then(|caps| caps.structure_mut(0)) {
                structure.set("a-msid", msid);
            }
            element.set_property("caps", &caps);
            let src = element.static_pad("src").ok_or_else(|| {
                WebRtcError::Backend("encoded media source has no src pad".into())
            })?;
            let (sink, transceiver_index) = self.sink_pad_for_transceiver(native)?;
            if let Some(stream_id) = track.stream_ids.first() {
                if sink.find_property("msid").is_some() {
                    sink.set_property("msid", stream_id);
                }
            }
            src.link(&sink)
                .map_err(|error| WebRtcError::Backend(error.to_string()))?;
            for element in branch {
                element
                    .sync_state_with_parent()
                    .map_err(|error| WebRtcError::Backend(error.to_string()))?;
            }
            if let Some(entry) = self
                .transceivers
                .lock()
                .unwrap()
                .by_id
                .get_mut(&transceiver_id)
            {
                entry.outgoing_ssrc = Some(outgoing_ssrc);
                entry.outgoing_payloader = Some(payloader);
                entry.outgoing_valve = Some(valve);
                entry.outgoing_capsfilter = Some(element);
            }
            self.request_pad_counter = self
                .request_pad_counter
                .max(transceiver_index.saturating_add(1) as usize);
            self.streams.push(stream_id);
            if is_internal_branch {
                self.internal_streams.push(stream_id);
            }
            Ok(())
        })();

        if result.is_err() && is_internal_branch {
            if let Err(error) = stop_stream(&stream_id) {
                warn!("Could not stop an unused WebRTC media branch: {error}");
            }
            unregister_stream(&stream_id);
        }
        result
    }

    fn apply_remote_answer_payloads(&mut self) -> WebRtcResult {
        let remote_mlines = &self.remote_mline_info;
        let mut transceivers = self.transceivers.lock().unwrap();
        for entry in transceivers.by_id.values_mut() {
            let (Some(payloader), Some(valve), Some(capsfilter), Some(mline_index)) = (
                entry.outgoing_payloader.as_ref(),
                entry.outgoing_valve.as_ref(),
                entry.outgoing_capsfilter.as_ref(),
                mline_index_for_transceiver(&entry.native),
            ) else {
                continue;
            };
            let Some(mline) = remote_mlines.get(mline_index as usize) else {
                continue;
            };
            let Some(payload) = negotiated_payload(mline, entry.kind) else {
                continue;
            };
            if entry.payload != Some(payload) {
                reconfigure_encoded_branch_payload(payloader, valve, capsfilter, payload)?;
                entry
                    .native
                    .set_property("codec-preferences", Some(codec_caps(entry.kind, payload)));
                entry.payload = Some(payload);
            } else {
                valve.set_property("drop", false);
            }
        }
        Ok(())
    }

    fn set_description(
        &mut self,
        desc: SessionDescription,
        description_type: DescriptionType,
        cb: Box<dyn FnOnce() + Send + 'static>,
    ) -> WebRtcResult {
        let ty = match desc.type_ {
            SdpType::Answer => gstreamer_webrtc::WebRTCSDPType::Answer,
            SdpType::Offer => gstreamer_webrtc::WebRTCSDPType::Offer,
            SdpType::Pranswer => gstreamer_webrtc::WebRTCSDPType::Pranswer,
            SdpType::Rollback => gstreamer_webrtc::WebRTCSDPType::Rollback,
        };

        let kind = match description_type {
            DescriptionType::Local => "set-local-description",
            DescriptionType::Remote => "set-remote-description",
        };

        let sdp = gstreamer_sdp::SDPMessage::parse_buffer(desc.sdp.as_bytes()).unwrap();
        if description_type == DescriptionType::Remote {
            self.remote_description_generation += 1;
            self.store_remote_mline_info(&sdp);
        } else {
            self.local_mline_count = sdp.medias().count();
        }
        let answer = gstreamer_webrtc::WebRTCSessionDescription::new(ty, sdp);
        let thread = self.thread.clone();
        let remote_description_generation = self.remote_description_generation;
        let promise = gstreamer::Promise::with_change_func(move |_promise| {
            // The generation prevents a stale remote-description callback from
            // replacing metadata prepared for a newer offer or answer.
            thread.internal_event(InternalEvent::DescriptionAdded(
                cb,
                description_type,
                desc.type_,
                remote_description_generation,
            ));
        });
        self.webrtc.emit_by_name::<()>(kind, &[&answer, &promise]);
        Ok(())
    }

    fn store_remote_mline_info(&mut self, sdp: &gstreamer_sdp::SDPMessage) {
        self.pending_remote_mline_info.clear();
        self.pending_remote_mline_metadata.clear();
        for media in sdp.medias() {
            let mut caps = gstreamer::Caps::new_empty();
            let caps_mut = caps.get_mut().expect("Fresh caps should be uniquely owned");
            let is_data_channel = media.formats().any(|format| format == "webrtc-datachannel");
            if is_data_channel {
                self.pending_remote_mline_info.push(MLineInfo {
                    caps,
                    // Data channels occupy SDP m-line positions but have no RTP sink pad.
                    is_used: true,
                    payload: -1,
                });
                self.pending_remote_mline_metadata
                    .push(RemoteMLineMetadata::default());
                continue;
            }
            for format in media.formats() {
                let pt = format
                    .parse()
                    .expect("Gstreamer provided noninteger format");
                caps_mut.append(
                    media
                        .caps_from_media(pt)
                        .expect("get_format() did not return a format from the SDP"),
                );
                self.pt_counter = cmp::max(self.pt_counter, pt + 1);
            }
            for s in caps_mut.iter_mut() {
                // the caps are application/x-unknown by default, which will fail
                // to intersect
                //
                // see https://gitlab.freedesktop.org/gstreamer/gst-plugins-bad/blob/ba62917fbfd98ea76d4e066a6f18b4a14b847362/ext/webrtc/gstwebrtcbin.c#L2521
                s.set_name("application/x-rtp")
            }
            // This info is not current until the promise from set-remote-description is resolved,
            // to avoid any races where we attempt to link streams before the promise resolves we
            // queue this up in a pending buffer
            self.pending_remote_mline_info.push(MLineInfo {
                caps,
                // XXXManishearth in the (yet unsupported) case of dynamic stream addition and renegotiation
                // this will need to be checked against the current set of streams
                is_used: false,
                // XXXManishearth ideally, we keep track of all payloads and have the capability of picking
                // the appropriate decoder. For this, a bunch of the streams code will have to be moved into
                // a webrtc-specific abstraction.
                payload: media
                    .format(0)
                    .expect("Gstreamer reported incorrect formats_len()")
                    .parse()
                    .expect("Gstreamer provided noninteger format"),
            });
            match remote_mline_metadata(media) {
                Ok(metadata) => self.pending_remote_mline_metadata.push(metadata),
                Err(error) => {
                    warn!("Ignoring ambiguous remote SDP m-line metadata: {error}");
                    self.pending_remote_mline_metadata
                        .push(RemoteMLineMetadata::default());
                },
            }
        }
    }

    /// Streams need to be linked to the correct pads, so we buffer them up until we know enough
    /// to do this.
    ///
    /// When we get a remote offer, we store the relevant m-line information so that we can
    /// pick the correct sink pad and payload. Shortly after we look for any pending streams
    /// and connect them to available compatible m-lines using link_stream.
    ///
    /// When we create an offer, we're controlling the pad order, so we set request_new_pads
    /// to true and forcefully link all pending streams before generating the offer.
    ///
    /// When request_new_pads is false, we may still request new pads, however we only do this for
    /// streams that have already been negotiated by the remote.
    fn link_stream(
        &mut self,
        stream_id: &MediaStreamId,
        stream: &mut GStreamerMediaStream,
        request_new_pads: bool,
    ) -> WebRtcResult {
        let caps = stream.caps();
        let idx = self
            .remote_mline_info
            .iter()
            .enumerate()
            .filter(|(_, x)| !x.is_used)
            .find(|(_, x)| x.caps.can_intersect(caps))
            .map(|x| x.0);
        if let Some(idx) = idx {
            if idx >= self.request_pad_counter {
                for i in self.request_pad_counter..=idx {
                    // webrtcbin needs you to request pads (or use element.link(webrtcbin))
                    // however, it also wants them to be connected in the correct order.
                    //
                    // Here, we make sure all the numbered sink pads have been created beforehand, up to
                    // and including the one we need here.
                    //
                    // An alternate fix is to sort pending_streams according to the m-line index
                    // and just do it in order. This also seems brittle.
                    self.webrtc
                        .request_pad_simple(&format!("sink_{}", i))
                        .ok_or("Cannot request sink pad")?;
                }
                self.request_pad_counter = idx + 1;
            }
            stream.attach_to_pipeline(&self.pipeline);
            let element = stream.encoded().map_err(|_| {
                WebRtcError::Backend(String::from("Failed to attach encoding adapters to stream"))
            })?;
            self.remote_mline_info[idx].is_used = true;
            let caps = stream.caps_with_payload(self.remote_mline_info[idx].payload);
            element.set_property("caps", &caps);
            let src = element.static_pad("src").ok_or("Cannot request src pad")?;
            let sink = self
                .webrtc
                .static_pad(&format!("sink_{}", idx))
                .ok_or("Cannot request sink pad")?;
            src.link(&sink)?;
            self.streams.push(*stream_id);
        } else if request_new_pads {
            stream.attach_to_pipeline(&self.pipeline);
            let element = stream.encoded().map_err(|_| {
                WebRtcError::Backend(String::from("Failed to attach encoding adapters to stream"))
            })?;
            let caps = stream.caps_with_payload(self.pt_counter);
            self.pt_counter += 1;
            element.set_property("caps", &caps);
            let src = element.static_pad("src").ok_or("Cannot request src pad")?;
            let sink = self
                .webrtc
                .request_pad_simple(&format!("sink_{}", self.request_pad_counter))
                .ok_or("Cannot request sink pad")?;
            self.request_pad_counter += 1;
            src.link(&sink)?;
            self.streams.push(*stream_id);
        } else {
            self.pending_streams.push(*stream_id);
        }
        Ok(())
    }

    /// link_stream, but for all pending streams
    fn flush_pending_streams(&mut self, request_new_pads: bool) -> WebRtcResult {
        let pending_streams = std::mem::take(&mut self.pending_streams);
        for stream_id in pending_streams {
            let stream =
                get_stream(&stream_id).expect("Media streams registry does not contain such ID");
            let mut stream = stream.lock().unwrap();
            let stream = stream
                .as_mut_any()
                .downcast_mut::<GStreamerMediaStream>()
                .ok_or("Does not currently support non-gstreamer streams")?;
            self.link_stream(&stream_id, stream, request_new_pads)?;
        }
        Ok(())
    }

    fn start_pipeline(&mut self) -> WebRtcResult {
        self.pipeline.add(&self.webrtc)?;

        // gstreamer needs Sync on these callbacks for some reason
        // https://github.com/sdroege/gstreamer-rs/issues/154
        let thread = Mutex::new(self.thread.clone());
        self.webrtc
            .connect("on-ice-candidate", false, move |values| {
                thread
                    .lock()
                    .unwrap()
                    .internal_event(InternalEvent::OnIceCandidate(candidate(values)));
                None
            });

        let thread = Arc::new(Mutex::new(self.thread.clone()));
        let remote_mline_metadata = self.remote_mline_metadata.clone();
        let transceivers = self.transceivers.clone();
        self.webrtc.connect_pad_added({
            let pipeline_weak = self.pipeline.downgrade();
            move |_element, pad| {
                let Some(pipe) = pipeline_weak.upgrade() else {
                    warn!("Pipeline already deallocated");
                    return;
                };
                process_new_stream(
                    pad,
                    &pipe,
                    thread.clone(),
                    remote_mline_metadata.clone(),
                    transceivers.clone(),
                );
            }
        });

        // gstreamer needs Sync on these callbacks for some reason
        // https://github.com/sdroege/gstreamer-rs/issues/154
        let thread = Mutex::new(self.thread.clone());
        self.webrtc
            .connect("on-negotiation-needed", false, move |_values| {
                thread
                    .lock()
                    .unwrap()
                    .internal_event(InternalEvent::OnNegotiationNeeded);
                None
            });

        let thread = Mutex::new(self.thread.clone());
        self.webrtc
            .connect("notify::signaling-state", false, move |_values| {
                thread
                    .lock()
                    .unwrap()
                    .internal_event(InternalEvent::UpdateSignalingState);
                None
            });
        let thread = Mutex::new(self.thread.clone());
        self.webrtc
            .connect("notify::connection-state", false, move |_values| {
                thread
                    .lock()
                    .unwrap()
                    .internal_event(InternalEvent::UpdateConnectionState);
                None
            });
        let thread = Mutex::new(self.thread.clone());
        self.webrtc
            .connect("notify::ice-connection-state", false, move |_values| {
                thread
                    .lock()
                    .unwrap()
                    .internal_event(InternalEvent::UpdateIceConnectionState);
                None
            });
        let thread = Mutex::new(self.thread.clone());
        self.webrtc
            .connect("notify::ice-gathering-state", false, move |_values| {
                thread
                    .lock()
                    .unwrap()
                    .internal_event(InternalEvent::UpdateGatheringState);
                None
            });
        let thread = Mutex::new(self.thread.clone());
        let data_channels = self.data_channels.clone();
        let next_data_channel_id = self.next_data_channel_id.clone();
        self.webrtc
            .connect("on-data-channel", false, move |channel| {
                let channel = channel[1]
                    .get::<gstreamer_webrtc::WebRTCDataChannel>()
                    .map_err(|e| e.to_string())
                    .expect("Invalid data channel");
                let id = next_data_channel_id.fetch_add(1, Ordering::Relaxed);
                let thread_ = thread.lock().unwrap().clone();
                match GStreamerWebRtcDataChannel::from(&id, channel, &thread_) {
                    Ok(channel) => {
                        let mut closed_channel = false;
                        {
                            thread_.internal_event(InternalEvent::OnDataChannelEvent(
                                id,
                                DataChannelEvent::NewChannel,
                            ));

                            let mut data_channels = data_channels.lock().unwrap();
                            if let Some(ref mut channel) = data_channels.get_mut(&id) {
                                match channel {
                                    &mut &mut DataChannelEventTarget::Buffered(ref mut events) => {
                                        for event in events.drain(0..) {
                                            if let DataChannelEvent::Close = event {
                                                closed_channel = true
                                            }
                                            thread_.internal_event(
                                                InternalEvent::OnDataChannelEvent(id, event),
                                            );
                                        }
                                    },
                                    _ => debug_assert!(
                                        false,
                                        "Trying to register a data channel with an existing ID"
                                    ),
                                }
                            }
                            data_channels.remove(&id);
                        }
                        if !closed_channel
                            && register_data_channel(data_channels.clone(), id, channel).is_err()
                        {
                            warn!("Could not register data channel {:?}", id);
                            return None;
                        }
                    },
                    Err(error) => {
                        warn!("Could not create data channel {:?}", error);
                    },
                }
                None
            });

        self.pipeline.set_state(gstreamer::State::Ready)?;
        Ok(())
    }
}

fn connection_state(
    state: gstreamer_webrtc::WebRTCPeerConnectionState,
) -> Result<ConnectionState, WebRtcError> {
    use gstreamer_webrtc::WebRTCPeerConnectionState as Gst;
    Ok(match state {
        Gst::New => ConnectionState::New,
        Gst::Connecting => ConnectionState::Connecting,
        Gst::Connected => ConnectionState::Connected,
        Gst::Disconnected => ConnectionState::Disconnected,
        Gst::Failed => ConnectionState::Failed,
        Gst::Closed => ConnectionState::Closed,
        other => {
            return Err(WebRtcError::Backend(format!(
                "Unknown connection state: {other:?}"
            )));
        },
    })
}

fn encoded_track_branch(
    stream: &mut GStreamerMediaStream,
    kind: TransceiverKind,
    payload: i32,
    ssrc: u32,
    audio_bitrate: Option<u32>,
) -> Result<
    (
        gstreamer::Element,
        gstreamer::Element,
        gstreamer::Element,
        Vec<gstreamer::Element>,
    ),
    WebRtcError,
> {
    let pipeline = stream.pipeline_or_new();
    let src = stream.src_element();
    let payload_type = u32::try_from(payload)
        .map_err(|_| WebRtcError::Backend("outgoing RTP payload is negative".into()))?;
    let (encoder, payloader) = match kind {
        TransceiverKind::Audio => {
            let encoder = match audio_bitrate {
                Some(bitrate) => gstreamer::ElementFactory::make("opusenc")
                    .property("bitrate", bitrate as i32)
                    .build(),
                None => gstreamer::ElementFactory::make("opusenc").build(),
            }
            .map_err(|error| WebRtcError::Backend(error.to_string()))?;
            let payloader = gstreamer::ElementFactory::make("rtpopuspay")
                .property("pt", payload_type)
                .property("ssrc", ssrc)
                .property("mtu", 1200u32)
                .build()
                .map_err(|error| WebRtcError::Backend(error.to_string()))?;
            (encoder, payloader)
        },
        TransceiverKind::Video => {
            if audio_bitrate.is_some() {
                return Err(WebRtcError::Backend(
                    "audio bitrate is only supported for audio tracks".into(),
                ));
            }
            let encoder = gstreamer::ElementFactory::make("vp8enc")
                .property("deadline", 1i64)
                .property("error-resilient", "default")
                .property("cpu-used", -16i32)
                .property("lag-in-frames", 0i32)
                .build()
                .map_err(|error| WebRtcError::Backend(error.to_string()))?;
            let payloader = gstreamer::ElementFactory::make("rtpvp8pay")
                .property("picture-id-mode", "15-bit")
                .property("pt", payload_type)
                .property("ssrc", ssrc)
                .property("mtu", 1200u32)
                .build()
                .map_err(|error| WebRtcError::Backend(error.to_string()))?;
            (encoder, payloader)
        },
    };
    let valve = gstreamer::ElementFactory::make("valve")
        .property("drop", true)
        .build()
        .map_err(|error| WebRtcError::Backend(error.to_string()))?;
    valve.set_property_from_str("drop-mode", "forward-sticky-events");
    let queue = gstreamer::ElementFactory::make("queue")
        .build()
        .map_err(|error| WebRtcError::Backend(error.to_string()))?;
    let capsfilter = gstreamer::ElementFactory::make("capsfilter")
        .property("caps", stream.caps())
        .build()
        .map_err(|error| WebRtcError::Backend(error.to_string()))?;

    pipeline
        .add_many([&valve, &encoder, &payloader, &queue, &capsfilter])
        .map_err(|error| WebRtcError::Backend(error.to_string()))?;
    gstreamer::Element::link_many([&src, &valve, &encoder, &payloader, &queue, &capsfilter])
        .map_err(|error| WebRtcError::Backend(error.to_string()))?;
    Ok((
        capsfilter.clone(),
        payloader.clone(),
        valve.clone(),
        vec![valve, encoder, payloader, queue, capsfilter],
    ))
}

fn mline_index_for_transceiver(
    transceiver: &gstreamer_webrtc::WebRTCRTPTransceiver,
) -> Option<u32> {
    let mline_index = transceiver.mlineindex();
    (mline_index != u32::MAX).then_some(mline_index)
}

fn negotiated_payload(mline: &MLineInfo, kind: TransceiverKind) -> Option<i32> {
    mline
        .caps
        .can_intersect(&codec_caps(kind, mline.payload))
        .then_some(mline.payload)
}

fn set_encoded_branch_payload(
    payloader: &gstreamer::Element,
    capsfilter: &gstreamer::Element,
    payload: i32,
) -> WebRtcResult {
    let payload_type = u32::try_from(payload)
        .map_err(|_| WebRtcError::Backend("negotiated RTP payload is negative".into()))?;
    payloader.set_property("pt", payload_type);

    let mut caps = capsfilter.property::<gstreamer::Caps>("caps");
    for structure in caps.make_mut().iter_mut() {
        structure.set("payload", payload);
    }
    capsfilter.set_property("caps", &caps);
    Ok(())
}

fn reconfigure_encoded_branch_payload(
    payloader: &gstreamer::Element,
    valve: &gstreamer::Element,
    capsfilter: &gstreamer::Element,
    payload: i32,
) -> WebRtcResult {
    // Keep the branch closed while changing the payloader PT and RTP caps.
    // The payloader property is not mutable in PLAYING, so update it while
    // NULL and synchronize it with the parent before releasing the valve.
    valve.set_property("drop", true);
    payloader
        .set_state(gstreamer::State::Null)
        .map_err(|error| WebRtcError::Backend(error.to_string()))?;
    set_encoded_branch_payload(payloader, capsfilter, payload)?;
    payloader
        .sync_state_with_parent()
        .map_err(|error| WebRtcError::Backend(error.to_string()))?;
    valve.set_property("drop", false);
    Ok(())
}

fn stream_for_pipeline(
    stream_id: &MediaStreamId,
    pipeline: &gstreamer::Pipeline,
) -> Result<(MediaStreamId, bool), WebRtcError> {
    let stream = get_stream(stream_id)
        .ok_or_else(|| WebRtcError::Backend("outgoing media stream does not exist".into()))?;
    let mut stream = stream.lock().unwrap();
    let source = stream
        .as_mut_any()
        .downcast_mut::<GStreamerMediaStream>()
        .ok_or_else(|| {
            WebRtcError::Backend("outgoing media stream is not backed by GStreamer".into())
        })?;

    let (linked_id, is_internal_branch) = match source.attached_pipeline() {
        Some(attached) if attached.as_ptr() == pipeline.as_ptr() => (*stream_id, false),
        Some(_) => (
            source.try_clone_branch().map_err(|error| {
                WebRtcError::Backend(format!("could not branch outgoing media stream: {error}"))
            })?,
            true,
        ),
        None => {
            source.attach_to_pipeline(pipeline);
            (*stream_id, false)
        },
    };
    drop(stream);

    if is_internal_branch {
        let branch = get_stream(&linked_id).ok_or_else(|| {
            WebRtcError::Backend("new outgoing media branch is not registered".into())
        })?;
        let mut branch = branch.lock().unwrap();
        let branch = branch
            .as_mut_any()
            .downcast_mut::<GStreamerMediaStream>()
            .ok_or_else(|| {
                WebRtcError::Backend("outgoing media branch is not backed by GStreamer".into())
            })?;
        branch.attach_to_pipeline(pipeline);
    }

    Ok((linked_id, is_internal_branch))
}

fn codec_caps(kind: TransceiverKind, payload: i32) -> gstreamer::Caps {
    match kind {
        TransceiverKind::Audio => gstreamer::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("encoding-name", "OPUS")
            .field("clock-rate", 48_000i32)
            .field("payload", payload)
            .build(),
        TransceiverKind::Video => gstreamer::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("encoding-name", "VP8")
            .field("clock-rate", 90_000i32)
            .field("payload", payload)
            .build(),
    }
}

fn native_direction(
    direction: TransceiverDirection,
) -> gstreamer_webrtc::WebRTCRTPTransceiverDirection {
    use gstreamer_webrtc::WebRTCRTPTransceiverDirection as Gst;
    match direction {
        TransceiverDirection::SendRecv => Gst::Sendrecv,
        TransceiverDirection::SendOnly => Gst::Sendonly,
        TransceiverDirection::RecvOnly => Gst::Recvonly,
        TransceiverDirection::Inactive => Gst::Inactive,
    }
}

fn transceiver_remote_kind(
    transceiver: &gstreamer_webrtc::WebRTCRTPTransceiver,
    remote_mlines: &[RemoteMLineMetadata],
) -> Option<TransceiverKind> {
    if let Some(mid) = transceiver.property::<Option<glib::GString>>("mid") {
        if let Some(metadata) = remote_mlines
            .iter()
            .find(|metadata| metadata.mid.as_deref() == Some(mid.as_str()))
        {
            return metadata.kind;
        }
    }

    let index = transceiver.mlineindex();
    (index != u32::MAX)
        .then(|| {
            remote_mlines
                .get(index as usize)
                .and_then(|metadata| metadata.kind)
        })
        .flatten()
}

pub fn construct(
    signaller: Box<dyn WebRtcSignaller>,
    thread: WebRtcThread,
) -> Result<GStreamerWebRtcController, WebRtcError> {
    let main_loop = glib::MainLoop::new(None, false);
    let pipeline = gstreamer::Pipeline::with_name("webrtc main");
    pipeline.set_start_time(gstreamer::ClockTime::NONE);
    pipeline.set_base_time(*BACKEND_BASE_TIME);
    pipeline.use_clock(Some(&gstreamer::SystemClock::obtain()));
    let webrtc = gstreamer::ElementFactory::make("webrtcbin")
        .name("sendrecv")
        .build()
        .map_err(|error| format!("webrtcbin element not found: {error:?}"))?;
    let mut controller = GStreamerWebRtcController {
        webrtc,
        pipeline,
        signaller,
        thread,
        remote_mline_info: vec![],
        remote_mline_metadata: Arc::new(Mutex::new(vec![])),
        transceivers: Arc::new(Mutex::new(TransceiverRegistry::default())),
        turn_servers: Vec::new(),
        pending_remote_mline_info: vec![],
        pending_remote_mline_metadata: vec![],
        local_mline_count: 0,
        streams: vec![],
        internal_streams: vec![],
        pending_streams: vec![],
        pt_counter: 96,
        request_pad_counter: 0,
        remote_description_generation: 0,
        delayed_negotiation: false,
        _main_loop: main_loop,
        data_channels: Arc::new(Mutex::new(HashMap::new())),
        next_data_channel_id: Arc::new(AtomicUsize::new(0)),
    };
    controller.start_pipeline()?;
    Ok(controller)
}

fn on_offer_or_answer_created(
    ty: SdpType,
    reply: &gstreamer::StructureRef,
    cb: Box<dyn FnOnce(SessionDescription) + Send + 'static>,
    transceivers: Option<Arc<Mutex<TransceiverRegistry>>>,
) {
    debug_assert!(ty == SdpType::Offer || ty == SdpType::Answer);
    let reply = reply
        .value(ty.as_str())
        .unwrap()
        .get::<gstreamer_webrtc::WebRTCSessionDescription>()
        .expect("Invalid argument");

    let type_ = match reply.type_() {
        gstreamer_webrtc::WebRTCSDPType::Answer => SdpType::Answer,
        gstreamer_webrtc::WebRTCSDPType::Offer => SdpType::Offer,
        gstreamer_webrtc::WebRTCSDPType::Pranswer => SdpType::Pranswer,
        gstreamer_webrtc::WebRTCSDPType::Rollback => SdpType::Rollback,
        _ => panic!("unknown sdp response"),
    };

    let mut sdp = reply.sdp().as_text().unwrap();
    if let Some(transceivers) = transceivers {
        let attributes: Vec<_> = transceivers
            .lock()
            .unwrap()
            .by_id
            .values()
            .filter_map(|entry| {
                let msid = entry.outgoing_msid.as_ref()?;
                let ssrc = entry.outgoing_ssrc?;
                Some((
                    entry.native.mlineindex(),
                    ssrc,
                    msid.stream_ids.clone(),
                    msid.track_id.clone(),
                ))
            })
            .collect();
        if !attributes.is_empty() {
            let mut message = gstreamer_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
                .expect("webrtcbin must return a parseable generated SDP");
            for (mline_index, ssrc, stream_ids, track_id) in attributes {
                let Some(media) = message.media_mut(mline_index) else {
                    warn!("Could not add outgoing stream IDs: SDP m-line {mline_index} is missing");
                    continue;
                };
                for stream_id in stream_ids.iter().skip(1) {
                    let value = format!("{stream_id} {track_id}");
                    media.add_attribute("msid", Some(&value));
                }
                let cname = format!("servo-media-{ssrc}");
                let cname_attribute = format!("{ssrc} cname:{cname}");
                media.add_attribute("ssrc", Some(&cname_attribute));
                let stream_ids: Vec<_> = if stream_ids.is_empty() {
                    vec!["-".to_owned()]
                } else {
                    stream_ids
                };
                for stream_id in stream_ids {
                    let msid_attribute = format!("{ssrc} msid:{stream_id} {track_id}");
                    media.add_attribute("ssrc", Some(&msid_attribute));
                }
            }
            sdp = message
                .as_text()
                .expect("the generated SDP must remain serializable after adding MSID attributes");
        }
    }

    let desc = SessionDescription { sdp, type_ };

    cb(desc);
}

fn on_incoming_stream(
    pipe: &gstreamer::Pipeline,
    thread: Arc<Mutex<WebRtcThread>>,
    pad: &gstreamer::Pad,
    remote_mline_metadata: Arc<Mutex<Vec<RemoteMLineMetadata>>>,
    transceivers: Arc<Mutex<TransceiverRegistry>>,
) {
    let metadata = match remote_track_metadata(
        pad,
        &remote_mline_metadata.lock().unwrap(),
        &transceivers,
    ) {
        Ok(metadata) => metadata,
        Err(error) => {
            warn!(
                "Ignoring an incoming WebRTC stream with inconsistent identity metadata: {error}"
            );
            return;
        },
    };
    let decodebin = match gstreamer::ElementFactory::make("decodebin").build() {
        Ok(decodebin) => decodebin,
        Err(error) => {
            warn!("Could not create decodebin for an incoming WebRTC stream: {error}");
            return;
        },
    };
    let decodebin2 = decodebin.clone();
    decodebin.connect_pad_added({
        let pipeline_weak = pipe.downgrade();
        move |_element, pad| {
            let Some(pipe) = pipeline_weak.upgrade() else {
                warn!("Pipeline already deallocated");
                return;
            };
            on_incoming_decodebin_stream(pad, &pipe, thread.clone(), metadata.clone());
        }
    });
    if let Err(error) = pipe.add(&decodebin) {
        warn!("Could not add decodebin for an incoming WebRTC stream: {error}");
        return;
    }

    let Some(decodepad) = decodebin.static_pad("sink") else {
        warn!("Incoming WebRTC decodebin has no sink pad");
        return;
    };
    if let Err(error) = pad.link(&decodepad) {
        warn!("Could not link an incoming WebRTC stream to decodebin: {error}");
        return;
    }
    if let Err(error) = decodebin2.sync_state_with_parent() {
        warn!("Could not start decodebin for an incoming WebRTC stream: {error}");
    }
}

fn on_incoming_decodebin_stream(
    pad: &gstreamer::Pad,
    pipe: &gstreamer::Pipeline,
    thread: Arc<Mutex<WebRtcThread>>,
    metadata: RemoteTrackMetadata,
) {
    // webrtcbin の RTP src pad は `application/x-rtp` だけを公開する場合があり、
    // SDP の media 属性を必ずしも caps で読めない。decodebin 後の negotiated caps なら
    // 実際に生成された raw stream の種類を表す。
    let Some(ty) = decoded_stream_type(pad) else {
        warn!(
            "Ignoring an incoming WebRTC stream with unsupported or unnegotiated decoded caps: {:?}",
            pad.current_caps(),
        );
        return;
    };
    let proxy_sink = match gstreamer::ElementFactory::make("proxysink").build() {
        Ok(proxy_sink) => proxy_sink,
        Err(error) => {
            warn!("Could not create proxysink for an incoming WebRTC stream: {error}");
            return;
        },
    };
    let proxy_src = match gstreamer::ElementFactory::make("proxysrc")
        .property("proxysink", &proxy_sink)
        .build()
    {
        Ok(proxy_src) => proxy_src,
        Err(error) => {
            warn!("Could not create proxysrc for an incoming WebRTC stream: {error}");
            return;
        },
    };
    if let Err(error) = pipe.add(&proxy_sink) {
        warn!("Could not add proxysink for an incoming WebRTC stream: {error}");
        return;
    }
    let Some(sinkpad) = proxy_sink.static_pad("sink") else {
        warn!("Incoming WebRTC proxysink has no sink pad");
        return;
    };

    if let Err(error) = pad.link(&sinkpad) {
        warn!("Could not link a decoded WebRTC stream to proxysink: {error}");
        return;
    }
    if let Err(error) = proxy_sink.sync_state_with_parent() {
        warn!("Could not start proxysink for an incoming WebRTC stream: {error}");
        return;
    }

    let stream = match ty {
        MediaStreamType::Video => GStreamerMediaStream::try_create_video_from(proxy_src),
        MediaStreamType::Audio => GStreamerMediaStream::try_create_audio_from(proxy_src),
    };
    let stream = match stream {
        Ok(stream) => stream,
        Err(error) => {
            warn!("Could not create a media stream for an incoming WebRTC track: {error}");
            return;
        },
    };
    thread
        .lock()
        .unwrap()
        .internal_event(InternalEvent::OnAddStream(stream, ty, metadata));
}

fn remote_mline_metadata(
    media: &gstreamer_sdp::SDPMediaRef,
) -> Result<RemoteMLineMetadata, String> {
    let mut stream_ids = Vec::new();
    let mut track_id = None;
    let mut ssrcs = Vec::new();

    for attribute in media.attributes() {
        let (msid, ssrc) = match attribute.key() {
            "msid" => (attribute.value(), None),
            "ssrc" => {
                let Some(value) = attribute.value() else {
                    continue;
                };
                let Some((ssrc, msid)) = value.split_once(" msid:") else {
                    continue;
                };
                let ssrc = ssrc
                    .parse::<u32>()
                    .map_err(|_| format!("an ssrc msid attribute has an invalid SSRC {ssrc:?}"))?;
                (Some(msid), Some(ssrc))
            },
            _ => continue,
        };
        let Some(msid) = msid else {
            continue;
        };
        let mut values = msid.split_ascii_whitespace();
        let Some(stream_id) = values.next() else {
            return Err("an msid attribute has no stream ID".into());
        };
        let Some(candidate_track_id) = values.next() else {
            return Err(format!(
                "an msid attribute for stream {stream_id:?} has no track ID"
            ));
        };
        if values.next().is_some() {
            return Err(format!(
                "an msid attribute for stream {stream_id:?} has extra values"
            ));
        }
        if stream_id != "-" && !stream_ids.iter().any(|id| id == stream_id) {
            stream_ids.push(stream_id.into());
        }
        if let Some(ssrc) = ssrc {
            if !ssrcs.contains(&ssrc) {
                ssrcs.push(ssrc);
            }
        }
        match &track_id {
            Some(existing) if existing != candidate_track_id => {
                return Err(format!(
                    "m-line advertises conflicting track IDs {existing:?} and {candidate_track_id:?}"
                ));
            },
            Some(_) => {},
            None => track_id = Some(candidate_track_id.into()),
        }
    }

    Ok(RemoteMLineMetadata {
        stream_ids,
        track_id,
        mid: media.attribute_val("mid").map(Into::into),
        kind: match media.media() {
            Some("audio") => Some(TransceiverKind::Audio),
            Some("video") => Some(TransceiverKind::Video),
            _ => None,
        },
        ssrcs,
    })
}

fn incoming_mline_for_rtp(
    remote_mlines: &[RemoteMLineMetadata],
    ssrc: Option<u32>,
    pad_mid: Option<String>,
    transceiver_mline_index: usize,
) -> Result<(usize, Option<String>), String> {
    if let Some(ssrc) = ssrc {
        let mut matching_mline = None;
        for (index, metadata) in remote_mlines.iter().enumerate() {
            if !metadata.ssrcs.contains(&ssrc) {
                continue;
            }
            if matching_mline.replace(index).is_some() {
                return Err(format!(
                    "incoming RTP SSRC {ssrc} is advertised by more than one SDP m-line"
                ));
            }
        }
        if let Some(index) = matching_mline {
            // GStreamer can publish a stale or incorrect a-mid on bundled RTP
            // pads. The SDP SSRC association identifies the actual m-line.
            return Ok((index, remote_mlines[index].mid.clone()));
        }
    }

    let mline_index = pad_mid
        .as_ref()
        .and_then(|mid| {
            remote_mlines
                .iter()
                .position(|metadata| metadata.mid.as_ref() == Some(mid))
        })
        .unwrap_or(transceiver_mline_index);
    Ok((mline_index, pad_mid))
}

fn ssrc_from_webrtc_stream_id(stream_id: &str) -> Option<u32> {
    let mut components = stream_id.rsplit('/');
    let payload = components.next()?.parse::<u8>().ok()?;
    if payload > 127 {
        return None;
    }
    let ssrc = components.next()?.parse::<u32>().ok()?;
    // webrtcbin formats its RTP stream IDs as `<stream>/<ssrc>/<payload>`.
    components.next()?;
    Some(ssrc)
}

fn remote_track_metadata(
    pad: &gstreamer::Pad,
    remote_mlines: &[RemoteMLineMetadata],
    transceivers: &Arc<Mutex<TransceiverRegistry>>,
) -> Result<RemoteTrackMetadata, String> {
    let pad_stream_id = pad.property::<Option<glib::GString>>("msid");
    let transceiver = pad
        .property::<Option<gstreamer_webrtc::WebRTCRTPTransceiver>>("transceiver")
        .ok_or_else(|| "incoming src pad has no transceiver".to_owned())?;
    let caps_mid = pad.current_caps().and_then(|caps| {
        caps.structure(0)
            .and_then(|s| s.get::<String>("a-mid").ok())
    });
    let caps_ssrc = pad
        .current_caps()
        .and_then(|caps| caps.structure(0).and_then(|s| s.get::<u32>("ssrc").ok()));
    let stream_start_ssrc = pad
        .sticky_event::<gstreamer::event::StreamStart>(0)
        .and_then(|event| ssrc_from_webrtc_stream_id(event.stream_id()));
    let native_mid = transceiver
        .property::<Option<glib::GString>>("mid")
        .map(|mid| mid.to_string());
    // GStreamer 1.28 can expose a BUNDLE source pad with a transceiver
    // property that points at the first m-line, while the RTP caps preserve
    // the MID of the actual incoming m-line. Use that MID to recover identity.
    let (mline_index, pad_mid) = incoming_mline_for_rtp(
        remote_mlines,
        caps_ssrc.or(stream_start_ssrc),
        caps_mid.or(native_mid),
        transceiver.property::<u32>("mlineindex") as usize,
    )?;
    let mline = remote_mlines
        .get(mline_index)
        .ok_or_else(|| format!("incoming pad refers to unknown m-line {mline_index}"))?;
    if let (Some(sdp_mid), Some(pad_mid)) = (&mline.mid, pad_mid.as_deref()) {
        if sdp_mid != pad_mid {
            return Err(format!(
                "incoming pad MID {pad_mid:?} conflicts with SDP MID {sdp_mid:?} for m-line {mline_index}"
            ));
        }
    }
    if let Some(pad_stream_id) = pad_stream_id.as_deref() {
        if !mline.stream_ids.is_empty() && !mline.stream_ids.iter().any(|id| id == &*pad_stream_id)
        {
            return Err(format!(
                "incoming pad msid {pad_stream_id:?} conflicts with SDP stream IDs {:?} for m-line {mline_index}",
                mline.stream_ids
            ));
        }
    }

    let stream_ids = if !mline.stream_ids.is_empty() {
        mline.stream_ids.clone()
    } else if pad_mid.is_none() {
        pad_stream_id
            .map(|id| vec![id.to_string()])
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let kind = mline
        .kind
        .ok_or_else(|| format!("incoming m-line {mline_index} has no supported media kind"))?;
    let transceiver_id = {
        let mut transceivers = transceivers.lock().unwrap();
        if let Some(mid) = pad_mid.as_deref() {
            transceivers.id_for_mid(mid).ok_or_else(|| {
                format!("no registered native transceiver matches remote MID {mid:?}")
            })?
        } else if let Some(id) = transceivers.id_for(&transceiver) {
            id
        } else {
            transceivers
                .register(transceiver, kind, None)
                .map_err(|error| format!("could not register incoming transceiver: {error:?}"))?
        }
    };
    Ok(RemoteTrackMetadata {
        stream_ids,
        track_id: mline.track_id.clone(),
        mid: pad_mid.or_else(|| mline.mid.clone()),
        transceiver_id,
    })
}

fn decoded_stream_type(pad: &gstreamer::Pad) -> Option<MediaStreamType> {
    let caps = pad.current_caps()?;
    let structure = caps.structure(0)?;
    match structure.name().as_str() {
        "audio/x-raw" => Some(MediaStreamType::Audio),
        "video/x-raw" => Some(MediaStreamType::Video),
        _ => None,
    }
}

fn process_new_stream(
    pad: &gstreamer::Pad,
    pipe: &gstreamer::Pipeline,
    thread: Arc<Mutex<WebRtcThread>>,
    remote_mline_metadata: Arc<Mutex<Vec<RemoteMLineMetadata>>>,
    transceivers: Arc<Mutex<TransceiverRegistry>>,
) {
    if pad.direction() != gstreamer::PadDirection::Src {
        // Ignore outgoing pad notifications.
        return;
    }
    on_incoming_stream(pipe, thread, pad, remote_mline_metadata, transceivers)
}

fn candidate(values: &[glib::Value]) -> IceCandidate {
    let _webrtc = values[0]
        .get::<gstreamer::Element>()
        .expect("Invalid argument");
    let sdp_mline_index = values[1].get::<u32>().expect("Invalid argument");
    let candidate = values[2].get::<String>().expect("Invalid argument");

    IceCandidate {
        sdp_mline_index,
        candidate,
    }
}

fn register_data_channel(
    registry: Arc<Mutex<HashMap<DataChannelId, DataChannelEventTarget>>>,
    id: DataChannelId,
    channel: GStreamerWebRtcDataChannel,
) -> WebRtcDataChannelResult {
    if registry.lock().unwrap().contains_key(&id) {
        return Err(WebRtcError::Backend(
            "Could not register data channel. ID collision".to_owned(),
        ));
    }
    registry
        .lock()
        .unwrap()
        .insert(id, DataChannelEventTarget::Created(channel));
    Ok(id)
}
