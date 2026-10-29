/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

extern crate log;
extern crate servo_media_streams;
extern crate uuid;

use std::fmt::Display;
use std::str::FromStr;

use servo_media_streams::MediaStreamType;
use servo_media_streams::registry::MediaStreamId;

pub mod datachannel;
pub mod ice;
pub mod thread;

pub use ice::{IceConfigError, IceConfiguration, IceServer, IceTransportPolicy};

pub use datachannel::{
    DataChannelEvent, DataChannelId, DataChannelInit, DataChannelMessage, DataChannelState,
};
pub use thread::WebRtcController;

/// Identity advertised by the remote peer for a received RTP track.
///
/// `stream_ids` and `track_id` are copied from the remote SDP `msid` data. A
/// streamless track is valid, so an empty `stream_ids` does not mean that the
/// metadata is unavailable. `track_id` is absent when the remote peer did not
/// advertise one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteTrackMetadata {
    pub stream_ids: Vec<String>,
    pub track_id: Option<String>,
    pub mid: Option<String>,
    /// Stable identity of the transceiver that delivered this track.
    pub transceiver_id: TransceiverId,
}

/// Stable identity for a transceiver during one peer connection's lifetime.
///
/// This is assigned independently of the SDP m-line index, which is not
/// available until negotiation has assigned one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TransceiverId(pub u64);

/// Media kind used to create a transceiver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransceiverKind {
    Audio,
    Video,
}

/// Sending and receiving direction for a transceiver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransceiverDirection {
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
}

/// A media source and its DOM identity for a track based transceiver.
#[derive(Clone, PartialEq, Eq)]
pub struct OutgoingTrack {
    /// Servo media stream registry entry that provides the encoded source.
    pub source_stream: MediaStreamId,
    /// DOM `MediaStream.id` values advertised for the track.
    pub stream_ids: Vec<String>,
    /// DOM `MediaStreamTrack.id` advertised in SDP.
    pub track_id: String,
    /// Optional Opus encoder target bitrate in bits per second.
    pub audio_bitrate: Option<u32>,
}

/// A snapshot of the native transceiver state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransceiverState {
    pub id: TransceiverId,
    pub kind: TransceiverKind,
    pub direction: TransceiverDirection,
    /// MID is absent until the transceiver has been assigned an SDP m-line.
    pub mid: Option<String>,
    /// M-line index is absent until negotiation assigns it.
    pub mline_index: Option<u16>,
}

#[derive(Clone, Debug)]
pub enum WebRtcError {
    Backend(String),
}

pub type WebRtcResult = Result<(), WebRtcError>;
pub type WebRtcDataChannelResult = Result<DataChannelId, WebRtcError>;

impl<T: Display> From<T> for WebRtcError {
    fn from(x: T) -> Self {
        WebRtcError::Backend(x.to_string())
    }
}

/// This trait is implemented by backends and should never be used directly by
/// the client. Use WebRtcController instead
pub trait WebRtcControllerBackend: Send {
    fn configure(&mut self, stun_server: &str, policy: BundlePolicy) -> WebRtcResult;
    fn set_ice_configuration(&mut self, _: IceConfiguration) -> WebRtcResult {
        Err(WebRtcError::Backend(
            "The WebRTC backend does not support ICE configuration".into(),
        ))
    }
    fn set_remote_description(
        &mut self,
        _: SessionDescription,
        cb: Box<dyn FnOnce() + Send + 'static>,
    ) -> WebRtcResult;
    fn set_local_description(
        &mut self,
        _: SessionDescription,
        cb: Box<dyn FnOnce() + Send + 'static>,
    ) -> WebRtcResult;
    fn add_ice_candidate(&mut self, candidate: IceCandidate) -> WebRtcResult;
    fn create_offer(
        &mut self,
        cb: Box<dyn FnOnce(SessionDescription) + Send + 'static>,
    ) -> WebRtcResult;
    fn create_answer(
        &mut self,
        cb: Box<dyn FnOnce(SessionDescription) + Send + 'static>,
    ) -> WebRtcResult;
    fn add_stream(&mut self, stream: &MediaStreamId) -> WebRtcResult;

    fn add_transceiver(
        &mut self,
        _: TransceiverKind,
        _: TransceiverDirection,
        _: Option<OutgoingTrack>,
    ) -> Result<TransceiverId, WebRtcError> {
        Err(WebRtcError::Backend(
            "The WebRTC backend does not support transceivers".into(),
        ))
    }

    fn set_transceiver_direction(
        &mut self,
        _: TransceiverId,
        _: TransceiverDirection,
    ) -> WebRtcResult {
        Err(WebRtcError::Backend(
            "The WebRTC backend does not support transceivers".into(),
        ))
    }

    fn get_transceivers(&mut self) -> Vec<TransceiverState> {
        Vec::new()
    }

    fn create_data_channel(&mut self, init: &DataChannelInit) -> WebRtcDataChannelResult;
    fn close_data_channel(&mut self, channel: &DataChannelId) -> WebRtcResult;
    fn send_data_channel_message(
        &mut self,
        channel: &DataChannelId,
        message: &DataChannelMessage,
    ) -> WebRtcResult;

    fn internal_event(&mut self, event: thread::InternalEvent) -> WebRtcResult;
    fn quit(&mut self);
}

pub trait WebRtcSignaller: Send {
    fn on_ice_candidate(&self, controller: &WebRtcController, candidate: IceCandidate);
    fn on_negotiation_needed(&self, controller: &WebRtcController);
    fn close(&self);
    fn on_add_stream(
        &self,
        stream: &MediaStreamId,
        ty: MediaStreamType,
        metadata: RemoteTrackMetadata,
    );

    fn update_signaling_state(&self, _: SignalingState) {}
    fn update_gathering_state(&self, _: GatheringState) {}
    fn update_ice_connection_state(&self, _: IceConnectionState) {}
    fn update_connection_state(&self, _: ConnectionState) {}

    fn on_data_channel_event(&self, _: DataChannelId, _: DataChannelEvent, _: &WebRtcController) {}
}

pub trait WebRtcBackend {
    type Controller: WebRtcControllerBackend + 'static;

    fn construct_webrtc_controller(
        signaller: Box<dyn WebRtcSignaller>,
        thread: WebRtcController,
    ) -> Self::Controller;
}

/// <https://www.w3.org/TR/webrtc/#rtcsdptype>
#[derive(Copy, Clone, Hash, Debug, PartialEq, Eq)]
pub enum SdpType {
    Answer,
    Offer,
    Pranswer,
    Rollback,
}

#[derive(Copy, Clone, Hash, Debug, PartialEq, Eq)]
pub enum DescriptionType {
    Local,
    Remote,
}

impl SdpType {
    pub fn as_str(self) -> &'static str {
        match self {
            SdpType::Answer => "answer",
            SdpType::Offer => "offer",
            SdpType::Pranswer => "pranswer",
            SdpType::Rollback => "rollback",
        }
    }
}

impl FromStr for SdpType {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        Ok(match s {
            "answer" => SdpType::Answer,
            "offer" => SdpType::Offer,
            "pranswer" => SdpType::Pranswer,
            "rollback" => SdpType::Rollback,
            _ => return Err(()),
        })
    }
}

/// <https://www.w3.org/TR/webrtc/#rtcsessiondescription-class>
///
/// <https://developer.mozilla.org/en-US/docs/Web/API/RTCSessionDescription>
#[derive(Clone, Hash, Debug, PartialEq, Eq)]
pub struct SessionDescription {
    pub type_: SdpType,
    pub sdp: String,
}

/// <https://www.w3.org/TR/webrtc/#rtcicecandidate-interface>
///
/// <https://developer.mozilla.org/en-US/docs/Web/API/RTCIceCandidate>
#[derive(Clone, Hash, Debug, PartialEq, Eq)]
pub struct IceCandidate {
    pub sdp_mline_index: u32,
    pub candidate: String,
    // XXXManishearth this is missing a bunch
}

/// <https://developer.mozilla.org/en-US/docs/Web/API/RTCPeerConnection#RTCBundlePolicy_enum>
#[derive(Clone, Copy, Hash, Debug, PartialEq, Eq)]
pub enum BundlePolicy {
    Balanced,
    MaxCompat,
    MaxBundle,
}

impl BundlePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            BundlePolicy::Balanced => "balanced",
            BundlePolicy::MaxCompat => "max-compat",
            BundlePolicy::MaxBundle => "max-bundle",
        }
    }
}

/// <https://www.w3.org/TR/webrtc/#rtcsignalingstate-enum>
#[derive(Clone, Copy, Hash, Debug, PartialEq, Eq)]
pub enum SignalingState {
    Stable,
    HaveLocalOffer,
    HaveRemoteOffer,
    HaveLocalPranswer,
    HaveRemotePranswer,
    Closed,
}

/// <https://www.w3.org/TR/webrtc/#rtcicegatheringstate-enum>
#[derive(Clone, Copy, Hash, Debug, PartialEq, Eq)]
pub enum GatheringState {
    New,
    Gathering,
    Complete,
}

/// <https://www.w3.org/TR/webrtc/#rtciceconnectionstate-enum>
#[derive(Clone, Copy, Hash, Debug, PartialEq, Eq)]
pub enum IceConnectionState {
    New,
    Checking,
    Connected,
    Completed,
    Disconnected,
    Failed,
    Closed,
}

/// ICE と DTLS の両方を含む接続状態。
#[derive(Clone, Copy, Hash, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}
