/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};

use crate::dom::bindings::codegen::Bindings::RTCRtpSenderBinding::{
    RTCRtcpParameters, RTCRtpParameters, RTCRtpSendParameters, RTCRtpSenderMethods,
};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::globalscope::GlobalScope;
use crate::dom::mediastreamtrack::MediaStreamTrack;
use crate::dom::{Promise, RootedPromise};

#[dom_struct]
pub(crate) struct RTCRtpSender {
    reflector_: Reflector,
    track: MutNullableDom<MediaStreamTrack>,
}

impl RTCRtpSender {
    fn new_inherited(track: Option<&MediaStreamTrack>) -> Self {
        Self {
            reflector_: Reflector::new(),
            track: MutNullableDom::new(track),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        track: Option<&MediaStreamTrack>,
    ) -> DomRoot<Self> {
        reflect_dom_object_with_cx(Box::new(Self::new_inherited(track)), global, cx)
    }
}

impl RTCRtpSenderMethods<crate::DomTypeHolder> for RTCRtpSender {
    /// <https://w3c.github.io/webrtc-pc/#dom-rtcrtpsender-track>
    fn GetTrack(&self) -> Option<DomRoot<MediaStreamTrack>> {
        self.track.get()
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcrtpsender-getparameters>
    fn GetParameters(&self) -> RTCRtpSendParameters {
        RTCRtpSendParameters {
            parent: RTCRtpParameters {
                headerExtensions: vec![],
                rtcp: RTCRtcpParameters {
                    cname: None,
                    reducedSize: None,
                },
                codecs: vec![],
            },
            transactionId: DOMString::new(),
            encodings: vec![],
        }
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcrtpsender-setparameters>
    fn SetParameters(
        &self,
        cx: &mut JSContext,
        _parameters: &RTCRtpSendParameters,
    ) -> RootedPromise {
        Promise::new_resolved(cx, &self.global(), ())
    }
}
