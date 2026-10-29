/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::mpsc::{Sender, channel};
use std::thread;

use log::error;
use servo_media_streams::MediaStreamType;

use crate::datachannel::{DataChannelEvent, DataChannelId, DataChannelInit, DataChannelMessage};
use crate::{
    BundlePolicy, DescriptionType, IceCandidate, IceConfiguration, MediaStreamId, OutgoingTrack,
    RemoteTrackMetadata, SdpType, SessionDescription, TransceiverDirection, TransceiverId,
    TransceiverKind, TransceiverState, WebRtcBackend, WebRtcControllerBackend, WebRtcError,
    WebRtcSignaller,
};

#[derive(Clone)]
/// Entry point for all client webrtc interactions.
pub struct WebRtcController {
    sender: Sender<RtcThreadEvent>,
}

impl WebRtcController {
    pub fn new<T: WebRtcBackend>(signaller: Box<dyn WebRtcSignaller>) -> Self {
        let (sender, receiver) = channel();

        let t = WebRtcController { sender };

        let mut controller = T::construct_webrtc_controller(signaller, t.clone());

        thread::spawn(move || {
            while let Ok(event) = receiver.recv() {
                if !handle_rtc_event(&mut controller, event) {
                    // shut down event loop
                    break;
                }
            }
        });

        t
    }
    pub fn configure(&self, stun_server: String, policy: BundlePolicy) {
        let _ = self
            .sender
            .send(RtcThreadEvent::ConfigureStun(stun_server, policy));
    }
    pub fn set_ice_configuration(&self, config: IceConfiguration) -> Result<(), WebRtcError> {
        let (reply, receiver) = channel();
        self.sender
            .send(RtcThreadEvent::SetIceConfiguration(config, reply))
            .map_err(|_| WebRtcError::Backend("WebRTC event thread has stopped".into()))?;
        receiver
            .recv()
            .map_err(|_| WebRtcError::Backend("WebRTC event thread has stopped".into()))?
    }
    pub fn set_remote_description(
        &self,
        desc: SessionDescription,
        cb: Box<dyn FnOnce() + Send + 'static>,
    ) {
        let _ = self
            .sender
            .send(RtcThreadEvent::SetRemoteDescription(desc, cb));
    }
    pub fn set_local_description(
        &self,
        desc: SessionDescription,
        cb: Box<dyn FnOnce() + Send + 'static>,
    ) {
        let _ = self
            .sender
            .send(RtcThreadEvent::SetLocalDescription(desc, cb));
    }
    pub fn add_ice_candidate(&self, candidate: IceCandidate) {
        let _ = self.sender.send(RtcThreadEvent::AddIceCandidate(candidate));
    }
    pub fn create_offer(&self, cb: Box<dyn FnOnce(SessionDescription) + Send + 'static>) {
        let _ = self.sender.send(RtcThreadEvent::CreateOffer(cb));
    }
    pub fn create_answer(&self, cb: Box<dyn FnOnce(SessionDescription) + Send + 'static>) {
        let _ = self.sender.send(RtcThreadEvent::CreateAnswer(cb));
    }
    pub fn add_stream(&self, stream: &MediaStreamId) {
        let _ = self.sender.send(RtcThreadEvent::AddStream(*stream));
    }
    pub fn add_transceiver(
        &self,
        kind: TransceiverKind,
        direction: TransceiverDirection,
        track: Option<OutgoingTrack>,
    ) -> Result<TransceiverId, WebRtcError> {
        let (reply, receiver) = channel();
        self.sender
            .send(RtcThreadEvent::AddTransceiver(
                kind, direction, track, reply,
            ))
            .map_err(|_| WebRtcError::Backend("WebRTC event thread has stopped".into()))?;
        receiver
            .recv()
            .map_err(|_| WebRtcError::Backend("WebRTC event thread has stopped".into()))?
    }
    pub fn set_transceiver_direction(
        &self,
        id: TransceiverId,
        direction: TransceiverDirection,
    ) -> Result<(), WebRtcError> {
        let (reply, receiver) = channel();
        self.sender
            .send(RtcThreadEvent::SetTransceiverDirection(
                id, direction, reply,
            ))
            .map_err(|_| WebRtcError::Backend("WebRTC event thread has stopped".into()))?;
        receiver
            .recv()
            .map_err(|_| WebRtcError::Backend("WebRTC event thread has stopped".into()))?
    }
    pub fn get_transceivers(&self) -> Result<Vec<TransceiverState>, WebRtcError> {
        let (reply, receiver) = channel();
        self.sender
            .send(RtcThreadEvent::GetTransceivers(reply))
            .map_err(|_| WebRtcError::Backend("WebRTC event thread has stopped".into()))?;
        receiver
            .recv()
            .map_err(|_| WebRtcError::Backend("WebRTC event thread has stopped".into()))
    }
    pub fn create_data_channel(&self, init: DataChannelInit) -> Option<DataChannelId> {
        let (sender, receiver) = channel();
        let _ = self
            .sender
            .send(RtcThreadEvent::CreateDataChannel(init, sender));
        receiver.recv().unwrap()
    }
    pub fn send_data_channel_message(&self, id: &DataChannelId, message: DataChannelMessage) {
        let _ = self
            .sender
            .send(RtcThreadEvent::SendDataChannelMessage(*id, message));
    }
    pub fn close_data_channel(&self, id: &DataChannelId) {
        let _ = self.sender.send(RtcThreadEvent::CloseDataChannel(*id));
    }

    /// This should not be invoked by clients
    pub fn internal_event(&self, event: InternalEvent) {
        let _ = self.sender.send(RtcThreadEvent::InternalEvent(event));
    }

    pub fn quit(&self) {
        let _ = self.sender.send(RtcThreadEvent::Quit);
    }
}

pub enum RtcThreadEvent {
    ConfigureStun(String, BundlePolicy),
    SetIceConfiguration(IceConfiguration, Sender<Result<(), WebRtcError>>),
    SetRemoteDescription(SessionDescription, Box<dyn FnOnce() + Send + 'static>),
    SetLocalDescription(SessionDescription, Box<dyn FnOnce() + Send + 'static>),
    AddIceCandidate(IceCandidate),
    CreateOffer(Box<dyn FnOnce(SessionDescription) + Send + 'static>),
    CreateAnswer(Box<dyn FnOnce(SessionDescription) + Send + 'static>),
    AddStream(MediaStreamId),
    AddTransceiver(
        TransceiverKind,
        TransceiverDirection,
        Option<OutgoingTrack>,
        Sender<Result<TransceiverId, WebRtcError>>,
    ),
    SetTransceiverDirection(
        TransceiverId,
        TransceiverDirection,
        Sender<Result<(), WebRtcError>>,
    ),
    GetTransceivers(Sender<Vec<TransceiverState>>),
    CreateDataChannel(DataChannelInit, Sender<Option<DataChannelId>>),
    CloseDataChannel(DataChannelId),
    SendDataChannelMessage(DataChannelId, DataChannelMessage),
    InternalEvent(InternalEvent),
    Quit,
}

/// To allow everything to occur on the event loop,
/// the backend may need to send signals to itself
///
/// This is a somewhat leaky abstraction, but we don't
/// plan on having too many backends anyway
pub enum InternalEvent {
    OnNegotiationNeeded,
    OnIceCandidate(IceCandidate),
    OnAddStream(MediaStreamId, MediaStreamType, RemoteTrackMetadata),
    OnDataChannelEvent(DataChannelId, DataChannelEvent),
    DescriptionAdded(
        Box<dyn FnOnce() + Send + 'static>,
        DescriptionType,
        SdpType,
        /* remote offer generation */ u32,
    ),
    UpdateSignalingState,
    UpdateGatheringState,
    UpdateIceConnectionState,
    UpdateConnectionState,
}

pub fn handle_rtc_event(
    controller: &mut dyn WebRtcControllerBackend,
    event: RtcThreadEvent,
) -> bool {
    let result = match event {
        RtcThreadEvent::ConfigureStun(server, policy) => controller.configure(&server, policy),
        RtcThreadEvent::SetIceConfiguration(config, sender) => {
            let result = controller.set_ice_configuration(config);
            let _ = sender.send(result.clone());
            result
        },
        RtcThreadEvent::SetRemoteDescription(desc, cb) => {
            controller.set_remote_description(desc, cb)
        },
        RtcThreadEvent::SetLocalDescription(desc, cb) => controller.set_local_description(desc, cb),
        RtcThreadEvent::AddIceCandidate(candidate) => controller.add_ice_candidate(candidate),
        RtcThreadEvent::CreateOffer(cb) => controller.create_offer(cb),
        RtcThreadEvent::CreateAnswer(cb) => controller.create_answer(cb),
        RtcThreadEvent::AddStream(media) => controller.add_stream(&media),
        RtcThreadEvent::AddTransceiver(kind, direction, track, sender) => {
            let result = controller.add_transceiver(kind, direction, track);
            let _ = sender.send(result.clone());
            result.map(|_| ())
        },
        RtcThreadEvent::SetTransceiverDirection(id, direction, sender) => {
            let result = controller.set_transceiver_direction(id, direction);
            let _ = sender.send(result.clone());
            result
        },
        RtcThreadEvent::GetTransceivers(sender) => {
            let _ = sender.send(controller.get_transceivers());
            Ok(())
        },
        RtcThreadEvent::CreateDataChannel(init, sender) => controller
            .create_data_channel(&init)
            .map(|id| {
                let _ = sender.send(Some(id));
            })
            .inspect_err(|_| {
                let _ = sender.send(None);
            }),
        RtcThreadEvent::CloseDataChannel(id) => controller.close_data_channel(&id),
        RtcThreadEvent::SendDataChannelMessage(id, message) => {
            controller.send_data_channel_message(&id, &message)
        },
        RtcThreadEvent::InternalEvent(e) => controller.internal_event(e),
        RtcThreadEvent::Quit => {
            controller.quit();
            return false;
        },
    };
    if let Err(e) = result {
        error!("WebRTC backend encountered error: {:?}", e);
    }
    true
}
