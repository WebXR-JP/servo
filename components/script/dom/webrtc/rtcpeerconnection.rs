/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::rc::Rc;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use js::rust::HandleObject;
use rustc_hash::FxHashMap;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_weak_referenceable_dom_object_with_proto;
use servo_media::ServoMedia;
use servo_media::streams::MediaStreamType;
use servo_media::streams::registry::MediaStreamId;
use servo_media::webrtc::ice::{IceConfigError, IceConfiguration, IceServer, IceTransportPolicy};
use servo_media::webrtc::{
    BundlePolicy, ConnectionState, DataChannelEvent, DataChannelId, DataChannelState,
    GatheringState, IceCandidate, IceConnectionState, OutgoingTrack, RemoteTrackMetadata, SdpType,
    SessionDescription, SignalingState, TransceiverDirection, TransceiverKind, WebRtcController,
    WebRtcSignaller,
};

use crate::conversions::Convert;
use crate::dom::bindings::codegen::Bindings::MediaStreamBinding::MediaStreamMethods;
use crate::dom::bindings::codegen::Bindings::MediaStreamTrackBinding::MediaStreamTrackMethods;
use crate::dom::bindings::codegen::Bindings::RTCDataChannelBinding::RTCDataChannelInit;
use crate::dom::bindings::codegen::Bindings::RTCIceCandidateBinding::RTCIceCandidateInit;
use crate::dom::bindings::codegen::Bindings::RTCPeerConnectionBinding::{
    RTCAnswerOptions, RTCBundlePolicy, RTCConfiguration, RTCIceConnectionState,
    RTCIceCredentialType, RTCIceGatheringState, RTCIceTransportPolicy, RTCOfferOptions,
    RTCPeerConnectionMethods, RTCPeerConnectionState, RTCRtpTransceiverInit, RTCSignalingState,
};
use crate::dom::bindings::codegen::Bindings::RTCRtpSenderBinding::RTCRtpSenderMethods;
use crate::dom::bindings::codegen::Bindings::RTCRtpTransceiverBinding::{
    RTCRtpTransceiverDirection, RTCRtpTransceiverMethods,
};
use crate::dom::bindings::codegen::Bindings::RTCSessionDescriptionBinding::{
    RTCSdpType, RTCSessionDescriptionInit,
};
use crate::dom::bindings::codegen::UnionTypes::{MediaStreamTrackOrString, StringOrStringSequence};
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::{Trusted, TrustedPromise};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::bindings::str::USVString;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::eventtarget::EventTarget;
use crate::dom::mediastream::MediaStream;
use crate::dom::mediastreamtrack::MediaStreamTrack;
use crate::dom::rtcdatachannel::RTCDataChannel;
use crate::dom::rtcdatachannelevent::RTCDataChannelEvent;
use crate::dom::rtcicecandidate::RTCIceCandidate;
use crate::dom::rtcpeerconnectioniceevent::RTCPeerConnectionIceEvent;
use crate::dom::rtcrtpreceiver::RTCRtpReceiver;
use crate::dom::rtcrtpsender::RTCRtpSender;
use crate::dom::rtcrtptransceiver::{RTCRtpTransceiver, backend_direction};
use crate::dom::rtcsessiondescription::RTCSessionDescription;
use crate::dom::rtctrackevent::RTCTrackEvent;
use crate::dom::window::Window;
use crate::dom::{Promise, RootedPromise, TracedPromise};
use crate::realms::enter_auto_realm;
use crate::tasks::task_source::SendableTaskSource;

#[dom_struct]
pub(crate) struct RTCPeerConnection {
    eventtarget: EventTarget,
    #[ignore_malloc_size_of = "defined in servo-media"]
    #[no_trace]
    controller: DomRefCell<Option<WebRtcController>>,
    closed: Cell<bool>,
    // Helps track state changes between the time createOffer/createAnswer
    // is called and resolved
    offer_answer_generation: Cell<u32>,
    offer_promises: DomRefCell<Vec<TracedPromise>>,
    answer_promises: DomRefCell<Vec<TracedPromise>>,
    local_description: MutNullableDom<RTCSessionDescription>,
    remote_description: MutNullableDom<RTCSessionDescription>,
    gathering_state: Cell<RTCIceGatheringState>,
    ice_connection_state: Cell<RTCIceConnectionState>,
    connection_state: Cell<RTCPeerConnectionState>,
    bundle_policy: Cell<RTCBundlePolicy>,
    #[ignore_malloc_size_of = "defined in servo-media"]
    #[no_trace]
    ice_config: DomRefCell<IceConfiguration>,
    signaling_state: Cell<RTCSignalingState>,
    #[ignore_malloc_size_of = "defined in servo-media"]
    data_channels: DomRefCell<FxHashMap<DataChannelId, Dom<RTCDataChannel>>>,
    senders: DomRefCell<Vec<Dom<RTCRtpSender>>>,
    transceivers: DomRefCell<Vec<Dom<RTCRtpTransceiver>>>,
}

struct RTCSignaller {
    trusted: Trusted<RTCPeerConnection>,
    task_source: SendableTaskSource,
}

fn copy_ice_configuration(config: &IceConfiguration) -> IceConfiguration {
    IceConfiguration {
        stun_server: config.stun_server.clone(),
        turn_servers: config.turn_servers.clone(),
        policy: config.policy,
    }
}

fn convert_ice_configuration(
    config: &RTCConfiguration,
    previous: Option<&IceConfiguration>,
) -> Fallible<IceConfiguration> {
    let policy = match config.iceTransportPolicy {
        RTCIceTransportPolicy::All => IceTransportPolicy::All,
        RTCIceTransportPolicy::Relay => IceTransportPolicy::Relay,
    };
    let Some(servers) = &config.iceServers else {
        let mut ice = previous.map(copy_ice_configuration).unwrap_or_default();
        ice.policy = policy;
        return Ok(ice);
    };
    let servers = servers
        .iter()
        .map(|server| {
            if server.credentialType != RTCIceCredentialType::Password {
                return Err(Error::NotSupported(Some(
                    "OAuth ICE credentials are not supported".into(),
                )));
            }
            let urls = match &server.urls {
                StringOrStringSequence::String(url) => vec![url.to_string()],
                StringOrStringSequence::StringSequence(urls) => {
                    urls.iter().map(ToString::to_string).collect()
                },
            };
            Ok(IceServer {
                urls,
                username: server.username.as_ref().map(ToString::to_string),
                credential: server.credential.as_ref().map(ToString::to_string),
            })
        })
        .collect::<Fallible<Vec<_>>>()?;
    IceConfiguration::new(servers, policy).map_err(|error| match error {
        IceConfigError::InvalidCredentials { .. } => Error::InvalidAccess(Some(error.to_string())),
        IceConfigError::MultipleStunServers | IceConfigError::UnsupportedScheme { .. } => {
            Error::NotSupported(Some(error.to_string()))
        },
        _ => Error::Syntax(Some(error.to_string())),
    })
}

impl WebRtcSignaller for RTCSignaller {
    fn on_ice_candidate(&self, _: &WebRtcController, candidate: IceCandidate) {
        let this = self.trusted.clone();
        self.task_source.queue(task!(on_ice_candidate: move |cx| {
            let this = this.root();
            this.on_ice_candidate(cx, candidate);
        }));
    }

    fn on_negotiation_needed(&self, _: &WebRtcController) {
        let this = self.trusted.clone();
        self.task_source
            .queue(task!(on_negotiation_needed: move |cx| {
                let this = this.root();
                this.on_negotiation_needed(cx);
            }));
    }

    fn update_gathering_state(&self, state: GatheringState) {
        let this = self.trusted.clone();
        self.task_source
            .queue(task!(update_gathering_state: move |cx| {
                let this = this.root();
                this.update_gathering_state(cx, state);
            }));
    }

    fn update_ice_connection_state(&self, state: IceConnectionState) {
        let this = self.trusted.clone();
        self.task_source
            .queue(task!(update_ice_connection_state: move |cx| {
                let this = this.root();
                this.update_ice_connection_state(cx, state);
            }));
    }

    fn update_connection_state(&self, state: ConnectionState) {
        let this = self.trusted.clone();
        self.task_source
            .queue(task!(update_connection_state: move |cx| {
                let this = this.root();
                this.update_connection_state(cx, state);
            }));
    }

    fn update_signaling_state(&self, state: SignalingState) {
        let this = self.trusted.clone();
        self.task_source
            .queue(task!(update_signaling_state: move |cx| {
                let this = this.root();
                this.update_signaling_state(cx, state);
            }));
    }

    fn on_add_stream(
        &self,
        id: &MediaStreamId,
        ty: MediaStreamType,
        metadata: RemoteTrackMetadata,
    ) {
        let this = self.trusted.clone();
        let id = *id;
        self.task_source.queue(task!(on_add_stream: move |cx| {
            let this = this.root();
            this.on_add_stream(cx, id, ty, metadata);
        }));
    }

    fn on_data_channel_event(
        &self,
        channel: DataChannelId,
        event: DataChannelEvent,
        _: &WebRtcController,
    ) {
        // XXX(ferjm) get label and options from channel properties.
        let this = self.trusted.clone();
        self.task_source
            .queue(task!(on_data_channel_event: move |cx| {
                let this = this.root();
                let global = this.global();
                let mut realm = enter_auto_realm(cx, &*global);
                this.on_data_channel_event(&mut realm.current_realm(), channel, event);
            }));
    }

    fn close(&self) {
        // do nothing
    }
}

impl RTCPeerConnection {
    pub(crate) fn new_inherited() -> RTCPeerConnection {
        RTCPeerConnection {
            eventtarget: EventTarget::new_inherited(),
            controller: DomRefCell::new(None),
            closed: Cell::new(false),
            offer_answer_generation: Cell::new(0),
            offer_promises: DomRefCell::new(vec![]),
            answer_promises: DomRefCell::new(vec![]),
            local_description: Default::default(),
            remote_description: Default::default(),
            gathering_state: Cell::new(RTCIceGatheringState::New),
            ice_connection_state: Cell::new(RTCIceConnectionState::New),
            connection_state: Cell::new(RTCPeerConnectionState::New),
            bundle_policy: Cell::new(RTCBundlePolicy::Balanced),
            ice_config: DomRefCell::new(IceConfiguration::default()),
            signaling_state: Cell::new(RTCSignalingState::Stable),
            data_channels: DomRefCell::new(FxHashMap::default()),
            senders: DomRefCell::new(vec![]),
            transceivers: DomRefCell::new(vec![]),
        }
    }

    fn new(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        config: &RTCConfiguration,
    ) -> Fallible<DomRoot<RTCPeerConnection>> {
        let ice_config = convert_ice_configuration(config, None)?;
        let this = reflect_weak_referenceable_dom_object_with_proto(
            cx,
            Rc::new(RTCPeerConnection::new_inherited()),
            window,
            proto,
        );
        let signaller = this.make_signaller();
        *this.controller.borrow_mut() = Some(ServoMedia::get().create_webrtc(signaller));
        let policy = match config.bundlePolicy {
            RTCBundlePolicy::Balanced => BundlePolicy::Balanced,
            RTCBundlePolicy::Max_compat => BundlePolicy::MaxCompat,
            RTCBundlePolicy::Max_bundle => BundlePolicy::MaxBundle,
        };
        let controller = this.controller.borrow().as_ref().unwrap().clone();
        controller.configure(String::new(), policy);
        if let Err(error) = controller.set_ice_configuration(copy_ice_configuration(&ice_config)) {
            controller.quit();
            return Err(Error::Operation(Some(format!("{error:?}").into())));
        }
        this.bundle_policy.set(config.bundlePolicy);
        *this.ice_config.borrow_mut() = ice_config;
        Ok(this)
    }

    pub(crate) fn get_webrtc_controller(&self) -> &DomRefCell<Option<WebRtcController>> {
        &self.controller
    }

    fn make_signaller(&self) -> Box<dyn WebRtcSignaller> {
        let trusted = Trusted::new(self);
        Box::new(RTCSignaller {
            trusted,
            task_source: self.global().task_manager().networking_task_source().into(),
        })
    }

    fn on_ice_candidate(&self, cx: &mut JSContext, candidate: IceCandidate) {
        if self.closed.get() {
            return;
        }
        let candidate = RTCIceCandidate::new(
            cx,
            self.global().as_window(),
            candidate.candidate.into(),
            None,
            Some(candidate.sdp_mline_index as u16),
            None,
        );
        let event = RTCPeerConnectionIceEvent::new(
            cx,
            self.global().as_window(),
            atom!("icecandidate"),
            Some(&candidate),
            None,
            true,
        );
        event.upcast::<Event>().fire(cx, self.upcast());
    }

    fn on_negotiation_needed(&self, cx: &mut JSContext) {
        if self.closed.get() {
            return;
        }
        let event = Event::new(
            cx,
            &self.global(),
            atom!("negotiationneeded"),
            EventBubbles::DoesNotBubble,
            EventCancelable::NotCancelable,
        );
        event.upcast::<Event>().fire(cx, self.upcast());
    }

    fn on_add_stream(
        &self,
        cx: &mut JSContext,
        id: MediaStreamId,
        ty: MediaStreamType,
        metadata: RemoteTrackMetadata,
    ) {
        if self.closed.get() {
            return;
        }
        let track = match metadata.track_id {
            Some(track_id) => {
                MediaStreamTrack::new_with_id(cx, &self.global(), id, ty, track_id.into())
            },
            None => MediaStreamTrack::new(cx, &self.global(), id, ty),
        };
        let receiver = RTCRtpReceiver::new(cx, &self.global(), &track);
        let streams = metadata
            .stream_ids
            .into_iter()
            .map(|stream_id| {
                let stream = MediaStream::new_with_id(cx, &self.global(), stream_id.into());
                stream.add_track(&track);
                stream
            })
            .collect::<Vec<_>>();
        let existing = self
            .transceivers
            .borrow()
            .iter()
            .find(|transceiver| transceiver.id() == metadata.transceiver_id)
            .map(|transceiver| DomRoot::from_ref(&**transceiver));
        let transceiver = if let Some(transceiver) = existing {
            transceiver.set_remote(&receiver, metadata.mid.map(Into::into));
            transceiver
        } else {
            let controller = self.controller.borrow().as_ref().unwrap().clone();
            let transceiver = RTCRtpTransceiver::new_remote(
                cx,
                &self.global(),
                RTCRtpTransceiverDirection::Recvonly,
                &receiver,
                metadata.mid.map(Into::into),
                controller,
                metadata.transceiver_id,
            );
            self.senders
                .borrow_mut()
                .push(Dom::from_ref(&*transceiver.Sender()));
            self.transceivers
                .borrow_mut()
                .push(Dom::from_ref(&*transceiver));
            transceiver
        };
        let event = RTCTrackEvent::new(
            cx,
            self.global().as_window(),
            atom!("track"),
            false,
            false,
            &track,
            &receiver,
            &transceiver,
            &streams,
        );
        event.upcast::<Event>().fire(cx, self.upcast());
    }

    fn on_data_channel_event(
        &self,
        cx: &mut CurrentRealm,
        channel_id: DataChannelId,
        event: DataChannelEvent,
    ) {
        if self.closed.get() {
            return;
        }

        match event {
            DataChannelEvent::NewChannel => {
                let channel = RTCDataChannel::new(
                    cx,
                    &self.global(),
                    self,
                    USVString::from(String::new()),
                    &RTCDataChannelInit::empty(),
                    Some(channel_id),
                );

                let event = RTCDataChannelEvent::new(
                    cx,
                    self.global().as_window(),
                    atom!("datachannel"),
                    false,
                    false,
                    &channel,
                );
                event.upcast::<Event>().fire(cx, self.upcast());
            },
            _ => {
                let channel: DomRoot<RTCDataChannel> =
                    if let Some(channel) = self.data_channels.borrow().get(&channel_id) {
                        DomRoot::from_ref(&**channel)
                    } else {
                        warn!(
                            "Got an event for an unregistered data channel {:?}",
                            channel_id
                        );
                        return;
                    };

                match event {
                    DataChannelEvent::Open => channel.on_open(cx),
                    DataChannelEvent::Close => channel.on_close(cx),
                    DataChannelEvent::Error(error) => channel.on_error(cx, error),
                    DataChannelEvent::OnMessage(message) => channel.on_message(cx, message),
                    DataChannelEvent::StateChange(state) => channel.on_state_change(cx, state),
                    DataChannelEvent::NewChannel => unreachable!(),
                }
            },
        };
    }

    pub(crate) fn register_data_channel(&self, id: DataChannelId, channel: &RTCDataChannel) {
        if self
            .data_channels
            .borrow_mut()
            .insert(id, Dom::from_ref(channel))
            .is_some()
        {
            warn!("Data channel already registered {:?}", id);
        }
    }

    pub(crate) fn unregister_data_channel(&self, id: &DataChannelId) {
        self.data_channels.borrow_mut().remove(id);
    }

    /// <https://www.w3.org/TR/webrtc/#update-ice-gathering-state>
    fn update_gathering_state(&self, cx: &mut JSContext, state: GatheringState) {
        // step 1
        if self.closed.get() {
            return;
        }

        // step 2 (state derivation already done by gstreamer)
        let state: RTCIceGatheringState = state.convert();

        // step 3
        if state == self.gathering_state.get() {
            return;
        }

        // step 4
        self.gathering_state.set(state);

        // step 5
        let event = Event::new(
            cx,
            &self.global(),
            atom!("icegatheringstatechange"),
            EventBubbles::DoesNotBubble,
            EventCancelable::NotCancelable,
        );
        event.upcast::<Event>().fire(cx, self.upcast());

        // step 6
        if state == RTCIceGatheringState::Complete {
            let event = RTCPeerConnectionIceEvent::new(
                cx,
                self.global().as_window(),
                atom!("icecandidate"),
                None,
                None,
                true,
            );
            event.upcast::<Event>().fire(cx, self.upcast());
        }
    }

    /// <https://www.w3.org/TR/webrtc/#update-ice-connection-state>
    fn update_ice_connection_state(&self, cx: &mut JSContext, state: IceConnectionState) {
        // step 1
        if self.closed.get() {
            return;
        }

        // step 2 (state derivation already done by gstreamer)
        let state: RTCIceConnectionState = state.convert();

        // step 3
        if state == self.ice_connection_state.get() {
            return;
        }

        // step 4
        self.ice_connection_state.set(state);

        // step 5
        let event = Event::new(
            cx,
            &self.global(),
            atom!("iceconnectionstatechange"),
            EventBubbles::DoesNotBubble,
            EventCancelable::NotCancelable,
        );
        event.upcast::<Event>().fire(cx, self.upcast());
    }

    fn update_connection_state(&self, cx: &mut JSContext, state: ConnectionState) {
        if self.closed.get() {
            return;
        }
        let state: RTCPeerConnectionState = state.convert();
        if state == self.connection_state.get() {
            return;
        }
        self.connection_state.set(state);
        let event = Event::new(
            cx,
            &self.global(),
            "connectionstatechange".into(),
            EventBubbles::DoesNotBubble,
            EventCancelable::NotCancelable,
        );
        event.upcast::<Event>().fire(cx, self.upcast());
    }

    fn update_signaling_state(&self, cx: &mut JSContext, state: SignalingState) {
        if self.closed.get() {
            return;
        }

        let state: RTCSignalingState = state.convert();

        if state == self.signaling_state.get() {
            return;
        }

        self.signaling_state.set(state);

        let event = Event::new(
            cx,
            &self.global(),
            atom!("signalingstatechange"),
            EventBubbles::DoesNotBubble,
            EventCancelable::NotCancelable,
        );
        event.upcast::<Event>().fire(cx, self.upcast());
    }

    fn create_offer(&self) {
        let generation = self.offer_answer_generation.get();
        let task_source = self
            .global()
            .task_manager()
            .networking_task_source()
            .to_sendable();
        let this = Trusted::new(self);
        self.controller
            .borrow_mut()
            .as_ref()
            .unwrap()
            .create_offer(Box::new(move |desc: SessionDescription| {
                task_source.queue(task!(offer_created: move |cx| {
                    let this = this.root();
                    if this.offer_answer_generation.get() != generation {
                        // the state has changed since we last created the offer,
                        // create a fresh one
                        this.create_offer();
                    } else {
                        let init: RTCSessionDescriptionInit = desc.convert();
                        for promise in this.offer_promises.borrow().iter() {
                            promise.resolve_native(cx, &init);
                        }
                        this.offer_promises.safe_borrow_mut(cx.no_gc()).clear();
                    }
                }));
            }));
    }

    fn create_answer(&self) {
        let generation = self.offer_answer_generation.get();
        let task_source = self
            .global()
            .task_manager()
            .networking_task_source()
            .to_sendable();
        let this = Trusted::new(self);
        self.controller
            .borrow_mut()
            .as_ref()
            .unwrap()
            .create_answer(Box::new(move |desc: SessionDescription| {
                task_source.queue(task!(answer_created: move |cx| {
                    let this = this.root();
                    if this.offer_answer_generation.get() != generation {
                        // the state has changed since we last created the offer,
                        // create a fresh one
                        this.create_answer();
                    } else {
                        let init: RTCSessionDescriptionInit = desc.convert();
                        for promise in this.answer_promises.borrow().iter() {
                            promise.resolve_native(cx, &init);
                        }
                        this.answer_promises.safe_borrow_mut(cx.no_gc()).clear();
                    }
                }));
            }));
    }
}

impl RTCPeerConnectionMethods<crate::DomTypeHolder> for RTCPeerConnection {
    /// <https://w3c.github.io/webrtc-pc/#dom-peerconnection>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        config: &RTCConfiguration,
    ) -> Fallible<DomRoot<RTCPeerConnection>> {
        RTCPeerConnection::new(cx, window, proto, config)
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-setconfiguration>
    fn SetConfiguration(&self, config: &RTCConfiguration) -> Fallible<()> {
        if self.closed.get() {
            return Err(Error::InvalidState(None));
        }
        if self.bundle_policy.get() != config.bundlePolicy {
            return Err(Error::InvalidModification(Some(
                "bundle policy cannot be changed".into(),
            )));
        }
        let ice_config = convert_ice_configuration(config, Some(&self.ice_config.borrow()))?;
        self.controller
            .borrow()
            .as_ref()
            .unwrap()
            .set_ice_configuration(copy_ice_configuration(&ice_config))
            .map_err(|error| Error::Operation(Some(format!("{error:?}").into())))?;
        *self.ice_config.borrow_mut() = ice_config;
        Ok(())
    }

    // https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-icecandidate
    event_handler!(icecandidate, GetOnicecandidate, SetOnicecandidate);

    // https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-ontrack
    event_handler!(track, GetOntrack, SetOntrack);

    // https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-iceconnectionstatechange
    event_handler!(
        iceconnectionstatechange,
        GetOniceconnectionstatechange,
        SetOniceconnectionstatechange
    );

    // https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-icegatheringstatechange
    event_handler!(
        icegatheringstatechange,
        GetOnicegatheringstatechange,
        SetOnicegatheringstatechange
    );

    // https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-onnegotiationneeded
    event_handler!(
        negotiationneeded,
        GetOnnegotiationneeded,
        SetOnnegotiationneeded
    );

    // https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-signalingstatechange
    event_handler!(
        signalingstatechange,
        GetOnsignalingstatechange,
        SetOnsignalingstatechange
    );

    // https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-ondatachannel
    event_handler!(datachannel, GetOndatachannel, SetOndatachannel);

    event_handler!(
        connectionstatechange,
        GetOnconnectionstatechange,
        SetOnconnectionstatechange
    );

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-addicecandidate>
    fn AddIceCandidate(
        &self,
        current_realm: &mut CurrentRealm,
        candidate: &RTCIceCandidateInit,
    ) -> RootedPromise {
        let p = Promise::new_in_realm(current_realm);
        if candidate.sdpMid.is_none() && candidate.sdpMLineIndex.is_none() {
            p.reject_error(
                current_realm,
                Error::Type(c"one of sdpMid and sdpMLineIndex must be set".to_owned()),
            );
            return p;
        }

        // XXXManishearth add support for sdpMid
        if candidate.sdpMLineIndex.is_none() {
            p.reject_error(
                current_realm,
                Error::Type(c"servo only supports sdpMLineIndex right now".to_owned()),
            );
            return p;
        }

        // XXXManishearth this should be enqueued
        // https://w3c.github.io/webrtc-pc/#enqueue-an-operation

        self.controller
            .borrow_mut()
            .as_ref()
            .unwrap()
            .add_ice_candidate(IceCandidate {
                sdp_mline_index: candidate.sdpMLineIndex.unwrap() as u32,
                candidate: candidate.candidate.to_string(),
            });

        // XXXManishearth add_ice_candidate should have a callback
        p.resolve_native(current_realm, &());
        p
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-createoffer>
    fn CreateOffer(
        &self,
        current_realm: &mut CurrentRealm,
        _options: &RTCOfferOptions,
    ) -> RootedPromise {
        let p = Promise::new_in_realm(current_realm);
        if self.closed.get() {
            p.reject_error(current_realm, Error::InvalidState(None));
            return p;
        }
        self.offer_promises.borrow_mut().push(p.to_traced());
        self.create_offer();
        p
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-createoffer>
    fn CreateAnswer(
        &self,
        current_realm: &mut CurrentRealm,
        _options: &RTCAnswerOptions,
    ) -> RootedPromise {
        let p = Promise::new_in_realm(current_realm);
        if self.closed.get() {
            p.reject_error(current_realm, Error::InvalidState(None));
            return p;
        }
        self.answer_promises.borrow_mut().push(p.to_traced());
        self.create_answer();
        p
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-localdescription>
    fn GetLocalDescription(&self) -> Option<DomRoot<RTCSessionDescription>> {
        self.local_description.get()
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-remotedescription>
    fn GetRemoteDescription(&self) -> Option<DomRoot<RTCSessionDescription>> {
        self.remote_description.get()
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-setlocaldescription>
    fn SetLocalDescription(
        &self,
        current_realm: &mut CurrentRealm,
        desc: &RTCSessionDescriptionInit,
    ) -> RootedPromise {
        // XXXManishearth validate the current state
        let p = Promise::new_in_realm(current_realm);
        let this = Trusted::new(self);
        let desc: SessionDescription = desc.convert();
        let trusted_promise = TrustedPromise::from(&p);
        let task_source = self
            .global()
            .task_manager()
            .networking_task_source()
            .to_sendable();
        self.controller
            .borrow()
            .as_ref()
            .unwrap()
            .set_local_description(
                desc.clone(),
                Box::new(move || {
                    task_source.queue(task!(local_description_set: move |current_realm| {
                        // XXXManishearth spec actually asks for an intricate
                        // dance between pending/current local/remote descriptions
                        let this = this.root();
                        let desc = desc.convert();
                        let desc = RTCSessionDescription::new(
                            current_realm,
                            this.global().as_window(),
                            None,
                            desc.type_,
                            desc.sdp,
                        );
                        this.local_description.set(Some(&desc));
                        trusted_promise.root(current_realm).resolve_native(current_realm, &())
                    }));
                }),
            );
        p
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-setremotedescription>
    fn SetRemoteDescription(
        &self,
        current_realm: &mut CurrentRealm,
        desc: &RTCSessionDescriptionInit,
    ) -> RootedPromise {
        // XXXManishearth validate the current state
        let p = Promise::new_in_realm(current_realm);
        let this = Trusted::new(self);
        let desc: SessionDescription = desc.convert();
        let trusted_promise = TrustedPromise::from(&p);
        let task_source = self
            .global()
            .task_manager()
            .networking_task_source()
            .to_sendable();
        self.controller
            .borrow()
            .as_ref()
            .unwrap()
            .set_remote_description(
                desc.clone(),
                Box::new(move || {
                    task_source.queue(task!(remote_description_set: move |current_realm| {
                        // XXXManishearth spec actually asks for an intricate
                        // dance between pending/current local/remote descriptions
                        let this = this.root();
                        let desc = desc.convert();
                        let desc = RTCSessionDescription::new(
                            current_realm,
                            this.global().as_window(),
                            None,
                            desc.type_,
                            desc.sdp,
                        );
                        this.remote_description.set(Some(&desc));
                        trusted_promise.root(current_realm).resolve_native(current_realm, &())
                    }));
                }),
            );
        p
    }

    /// <https://w3c.github.io/webrtc-pc/#legacy-interface-extensions>
    fn AddStream(&self, stream: &MediaStream) {
        for track in &*stream.get_tracks() {
            self.controller
                .borrow()
                .as_ref()
                .unwrap()
                .add_stream(&track.id());
        }
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-getsenders>
    fn GetSenders(&self) -> Vec<DomRoot<RTCRtpSender>> {
        self.senders
            .borrow()
            .iter()
            .map(|sender| DomRoot::from_ref(&**sender))
            .collect()
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-gettransceivers>
    fn GetTransceivers(&self) -> Vec<DomRoot<RTCRtpTransceiver>> {
        if !self.closed.get()
            && let Some(controller) = self.controller.borrow().as_ref()
            && let Ok(states) = controller.get_transceivers()
        {
            for state in &states {
                if let Some(transceiver) = self
                    .transceivers
                    .borrow()
                    .iter()
                    .find(|t| t.id() == state.id)
                {
                    transceiver.update_state(state);
                }
            }
        }
        self.transceivers
            .borrow()
            .iter()
            .map(|transceiver| DomRoot::from_ref(&**transceiver))
            .collect()
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-addtrack>
    fn AddTrack(
        &self,
        cx: &mut JSContext,
        track: &MediaStreamTrack,
        streams: &[&MediaStream],
    ) -> Fallible<DomRoot<RTCRtpSender>> {
        if self.closed.get() {
            return Err(Error::InvalidState(None));
        }
        if self
            .senders
            .borrow()
            .iter()
            .filter_map(|sender| sender.GetTrack())
            .any(|sender_track| &*sender_track == track)
        {
            return Err(Error::InvalidAccess(Some(
                "track is already associated with this connection".into(),
            )));
        }

        let controller = self.controller.borrow().as_ref().unwrap().clone();
        let source = OutgoingTrack {
            source_stream: track.id(),
            stream_ids: streams
                .iter()
                .map(|stream| stream.Id().to_string())
                .collect(),
            track_id: track.Id().to_string(),
            audio_bitrate: None,
        };
        let kind = match track.ty() {
            MediaStreamType::Audio => TransceiverKind::Audio,
            MediaStreamType::Video => TransceiverKind::Video,
        };
        let id = controller
            .add_transceiver(kind, TransceiverDirection::SendRecv, Some(source))
            .map_err(|error| Error::Operation(Some(format!("{error:?}").into())))?;
        let transceiver = RTCRtpTransceiver::new(
            cx,
            &self.global(),
            RTCRtpTransceiverDirection::Sendrecv,
            Some(track),
            controller,
            id,
        );
        let sender = transceiver.Sender();
        self.senders.borrow_mut().push(Dom::from_ref(&*sender));
        self.transceivers
            .borrow_mut()
            .push(Dom::from_ref(&*transceiver));
        Ok(sender)
    }

    /// <https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-icegatheringstate>
    fn IceGatheringState(&self) -> RTCIceGatheringState {
        self.gathering_state.get()
    }

    /// <https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-iceconnectionstate>
    fn IceConnectionState(&self) -> RTCIceConnectionState {
        self.ice_connection_state.get()
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-connectionstate>
    fn ConnectionState(&self) -> RTCPeerConnectionState {
        self.connection_state.get()
    }

    /// <https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-signalingstate>
    fn SignalingState(&self) -> RTCSignalingState {
        self.signaling_state.get()
    }

    /// <https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-close>
    fn Close(&self, cx: &mut JSContext) {
        // Step 1
        if self.closed.get() {
            return;
        }
        // Step 2
        self.closed.set(true);

        // Step 4
        self.signaling_state.set(RTCSignalingState::Closed);

        // Step 5 handled by backend
        self.controller.borrow_mut().as_ref().unwrap().quit();

        // Step 6
        for val in self.data_channels.borrow().values() {
            val.on_state_change(cx, DataChannelState::Closed);
        }

        // Step 7-10
        // (no current support for transports, etc)

        // Step 11
        self.ice_connection_state.set(RTCIceConnectionState::Closed);
        self.connection_state.set(RTCPeerConnectionState::Closed);
    }

    /// <https://www.w3.org/TR/webrtc/#dom-peerconnection-createdatachannel>
    fn CreateDataChannel(
        &self,
        cx: &mut JSContext,
        label: USVString,
        init: &RTCDataChannelInit,
    ) -> DomRoot<RTCDataChannel> {
        RTCDataChannel::new(cx, &self.global(), self, label, init, None)
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-addtransceiver>
    fn AddTransceiver(
        &self,
        cx: &mut JSContext,
        track_or_kind: MediaStreamTrackOrString,
        init: &RTCRtpTransceiverInit,
    ) -> Fallible<DomRoot<RTCRtpTransceiver>> {
        if self.closed.get() {
            return Err(Error::InvalidState(None));
        }
        let (kind, track) = match track_or_kind {
            MediaStreamTrackOrString::MediaStreamTrack(track) => {
                let kind = match track.ty() {
                    MediaStreamType::Audio => TransceiverKind::Audio,
                    MediaStreamType::Video => TransceiverKind::Video,
                };
                (kind, Some(track))
            },
            MediaStreamTrackOrString::String(kind) => match &*kind.str() {
                "audio" => (TransceiverKind::Audio, None),
                "video" => (TransceiverKind::Video, None),
                _ => return Err(Error::Type(c"Invalid transceiver kind".to_owned())),
            },
        };
        if track.as_ref().is_some_and(|track| {
            self.senders
                .borrow()
                .iter()
                .filter_map(|sender| sender.GetTrack())
                .any(|sender_track| &*sender_track == &**track)
        }) {
            return Err(Error::NotSupported(Some(
                "sending the same track on multiple transceivers is not supported".into(),
            )));
        }
        if init.sendEncodings.len() > 1
            || init.sendEncodings.iter().any(|encoding| {
                !encoding.active
                    || encoding.parent.rid.is_some()
                    || encoding.scaleResolutionDownBy.is_some()
            })
        {
            return Err(Error::NotSupported(Some(
                "unsupported RTP encoding options".into(),
            )));
        }
        let audio_bitrate = init
            .sendEncodings
            .first()
            .and_then(|encoding| encoding.maxBitrate);
        if kind == TransceiverKind::Video && audio_bitrate.is_some() {
            return Err(Error::NotSupported(Some(
                "video bitrate control is not supported".into(),
            )));
        }
        let source = track.as_ref().map(|track| OutgoingTrack {
            source_stream: track.id(),
            stream_ids: init
                .streams
                .iter()
                .map(|stream| stream.Id().to_string())
                .collect(),
            track_id: track.Id().to_string(),
            audio_bitrate,
        });
        let controller = self.controller.borrow().as_ref().unwrap().clone();
        let id = controller
            .add_transceiver(kind, backend_direction(init.direction)?, source)
            .map_err(|error| Error::Operation(Some(format!("{error:?}").into())))?;
        let transceiver = RTCRtpTransceiver::new(
            cx,
            &self.global(),
            init.direction,
            track.as_deref(),
            controller,
            id,
        );
        let sender = transceiver.Sender();
        self.senders.borrow_mut().push(Dom::from_ref(&*sender));
        self.transceivers
            .borrow_mut()
            .push(Dom::from_ref(&*transceiver));
        Ok(transceiver)
    }
}

impl Convert<RTCSessionDescriptionInit> for SessionDescription {
    fn convert(self) -> RTCSessionDescriptionInit {
        let type_ = match self.type_ {
            SdpType::Answer => RTCSdpType::Answer,
            SdpType::Offer => RTCSdpType::Offer,
            SdpType::Pranswer => RTCSdpType::Pranswer,
            SdpType::Rollback => RTCSdpType::Rollback,
        };
        RTCSessionDescriptionInit {
            type_,
            sdp: self.sdp.into(),
        }
    }
}

impl Convert<SessionDescription> for &RTCSessionDescriptionInit {
    fn convert(self) -> SessionDescription {
        let type_ = match self.type_ {
            RTCSdpType::Answer => SdpType::Answer,
            RTCSdpType::Offer => SdpType::Offer,
            RTCSdpType::Pranswer => SdpType::Pranswer,
            RTCSdpType::Rollback => SdpType::Rollback,
        };
        SessionDescription {
            type_,
            sdp: self.sdp.to_string(),
        }
    }
}

impl Convert<RTCIceGatheringState> for GatheringState {
    fn convert(self) -> RTCIceGatheringState {
        match self {
            GatheringState::New => RTCIceGatheringState::New,
            GatheringState::Gathering => RTCIceGatheringState::Gathering,
            GatheringState::Complete => RTCIceGatheringState::Complete,
        }
    }
}

impl Convert<RTCIceConnectionState> for IceConnectionState {
    fn convert(self) -> RTCIceConnectionState {
        match self {
            IceConnectionState::New => RTCIceConnectionState::New,
            IceConnectionState::Checking => RTCIceConnectionState::Checking,
            IceConnectionState::Connected => RTCIceConnectionState::Connected,
            IceConnectionState::Completed => RTCIceConnectionState::Completed,
            IceConnectionState::Disconnected => RTCIceConnectionState::Disconnected,
            IceConnectionState::Failed => RTCIceConnectionState::Failed,
            IceConnectionState::Closed => RTCIceConnectionState::Closed,
        }
    }
}

impl Convert<RTCPeerConnectionState> for ConnectionState {
    fn convert(self) -> RTCPeerConnectionState {
        match self {
            ConnectionState::New => RTCPeerConnectionState::New,
            ConnectionState::Connecting => RTCPeerConnectionState::Connecting,
            ConnectionState::Connected => RTCPeerConnectionState::Connected,
            ConnectionState::Disconnected => RTCPeerConnectionState::Disconnected,
            ConnectionState::Failed => RTCPeerConnectionState::Failed,
            ConnectionState::Closed => RTCPeerConnectionState::Closed,
        }
    }
}

impl Convert<RTCSignalingState> for SignalingState {
    fn convert(self) -> RTCSignalingState {
        match self {
            SignalingState::Stable => RTCSignalingState::Stable,
            SignalingState::HaveLocalOffer => RTCSignalingState::Have_local_offer,
            SignalingState::HaveRemoteOffer => RTCSignalingState::Have_remote_offer,
            SignalingState::HaveLocalPranswer => RTCSignalingState::Have_local_pranswer,
            SignalingState::HaveRemotePranswer => RTCSignalingState::Have_remote_pranswer,
            SignalingState::Closed => RTCSignalingState::Closed,
        }
    }
}
