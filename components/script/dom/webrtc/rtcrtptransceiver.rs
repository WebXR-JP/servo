/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use servo_media::webrtc::{
    TransceiverDirection, TransceiverId, TransceiverState, WebRtcController,
};

use crate::dom::bindings::codegen::Bindings::RTCRtpTransceiverBinding::{
    RTCRtpTransceiverDirection, RTCRtpTransceiverMethods,
};
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::globalscope::GlobalScope;
use crate::dom::mediastreamtrack::MediaStreamTrack;
use crate::dom::rtcrtpreceiver::RTCRtpReceiver;
use crate::dom::rtcrtpsender::RTCRtpSender;

#[dom_struct]
pub(crate) struct RTCRtpTransceiver {
    reflector_: Reflector,
    sender: Dom<RTCRtpSender>,
    receiver: MutNullableDom<RTCRtpReceiver>,
    mid: RefCell<Option<DOMString>>,
    direction: Cell<RTCRtpTransceiverDirection>,
    #[ignore_malloc_size_of = "defined in servo-media"]
    #[no_trace]
    controller: WebRtcController,
    #[ignore_malloc_size_of = "defined in servo-media"]
    #[no_trace]
    id: TransceiverId,
}

impl RTCRtpTransceiver {
    fn new_inherited(
        cx: &mut JSContext,
        global: &GlobalScope,
        direction: RTCRtpTransceiverDirection,
        track: Option<&MediaStreamTrack>,
        controller: WebRtcController,
        id: TransceiverId,
    ) -> Self {
        let sender = RTCRtpSender::new(cx, global, track);
        Self {
            reflector_: Reflector::new(),
            direction: Cell::new(direction),
            sender: Dom::from_ref(&*sender),
            controller,
            id,
            receiver: MutNullableDom::new(None),
            mid: RefCell::new(None),
        }
    }

    pub(crate) fn new_remote(
        cx: &mut JSContext,
        global: &GlobalScope,
        direction: RTCRtpTransceiverDirection,
        receiver: &RTCRtpReceiver,
        mid: Option<DOMString>,
        controller: WebRtcController,
        id: TransceiverId,
    ) -> DomRoot<Self> {
        let transceiver = Self::new(cx, global, direction, None, controller, id);
        transceiver.set_remote(receiver, mid);
        transceiver
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        direction: RTCRtpTransceiverDirection,
        track: Option<&MediaStreamTrack>,
        controller: WebRtcController,
        id: TransceiverId,
    ) -> DomRoot<Self> {
        reflect_dom_object_with_cx(
            Box::new(Self::new_inherited(
                cx, global, direction, track, controller, id,
            )),
            global,
            cx,
        )
    }

    pub(crate) fn id(&self) -> TransceiverId {
        self.id
    }

    pub(crate) fn set_remote(&self, receiver: &RTCRtpReceiver, mid: Option<DOMString>) {
        self.receiver.set(Some(receiver));
        *self.mid.borrow_mut() = mid;
    }

    pub(crate) fn update_state(&self, state: &TransceiverState) {
        *self.mid.borrow_mut() = state.mid.as_ref().map(|mid| mid.clone().into());
        self.direction.set(match state.direction {
            TransceiverDirection::SendRecv => RTCRtpTransceiverDirection::Sendrecv,
            TransceiverDirection::SendOnly => RTCRtpTransceiverDirection::Sendonly,
            TransceiverDirection::RecvOnly => RTCRtpTransceiverDirection::Recvonly,
            TransceiverDirection::Inactive => RTCRtpTransceiverDirection::Inactive,
        });
    }
}

pub(crate) fn backend_direction(
    direction: RTCRtpTransceiverDirection,
) -> Fallible<TransceiverDirection> {
    match direction {
        RTCRtpTransceiverDirection::Sendrecv => Ok(TransceiverDirection::SendRecv),
        RTCRtpTransceiverDirection::Sendonly => Ok(TransceiverDirection::SendOnly),
        RTCRtpTransceiverDirection::Recvonly => Ok(TransceiverDirection::RecvOnly),
        RTCRtpTransceiverDirection::Inactive => Ok(TransceiverDirection::Inactive),
        RTCRtpTransceiverDirection::Stopped => {
            Err(Error::Type(c"Cannot set direction to stopped".to_owned()))
        },
    }
}

impl RTCRtpTransceiverMethods<crate::DomTypeHolder> for RTCRtpTransceiver {
    /// <https://w3c.github.io/webrtc-pc/#dom-rtcrtptransceiver-direction>
    fn Direction(&self) -> RTCRtpTransceiverDirection {
        self.direction.get()
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcrtptransceiver-direction>
    fn SetDirection(&self, direction: RTCRtpTransceiverDirection) -> Fallible<()> {
        self.controller
            .set_transceiver_direction(self.id, backend_direction(direction)?)
            .map_err(|error| Error::Operation(Some(format!("{error:?}").into())))?;
        self.direction.set(direction);
        Ok(())
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcrtptransceiver-sender>
    fn Sender(&self) -> DomRoot<RTCRtpSender> {
        DomRoot::from_ref(&*self.sender)
    }

    fn GetReceiver(&self) -> Option<DomRoot<RTCRtpReceiver>> {
        self.receiver.get()
    }

    fn GetMid(&self) -> Option<DOMString> {
        self.mid.borrow().clone()
    }
}
