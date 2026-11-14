/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use servo_media::streams::MediaStreamType;
use servo_media::webrtc::{OutgoingTrack, TransceiverId, TransceiverKind, WebRtcController};

use crate::dom::bindings::codegen::Bindings::MediaStreamTrackBinding::MediaStreamTrackMethods;
use crate::dom::bindings::codegen::Bindings::RTCRtpSenderBinding::{
    RTCRtcpParameters, RTCRtpParameters, RTCRtpSendParameters, RTCRtpSenderMethods,
};
use crate::dom::bindings::error::Error;
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
    #[ignore_malloc_size_of = "defined in servo-media"]
    #[no_trace]
    controller: WebRtcController,
    #[ignore_malloc_size_of = "defined in servo-media"]
    #[no_trace]
    id: TransceiverId,
    #[ignore_malloc_size_of = "defined in servo-media"]
    #[no_trace]
    kind: TransceiverKind,
    /// AddTrack/AddTransceiver 時に渡された DOM Stream ID。SDP の msid を変えず
    /// backend へ渡す。replaceTrack でも維持する。
    stream_ids: DomRefCell<Vec<String>>,
    closed: Cell<bool>,
}

impl RTCRtpSender {
    fn new_inherited(
        track: Option<&MediaStreamTrack>,
        controller: WebRtcController,
        id: TransceiverId,
        kind: TransceiverKind,
        stream_ids: Vec<String>,
    ) -> Self {
        Self {
            reflector_: Reflector::new(),
            track: MutNullableDom::new(track),
            controller,
            id,
            kind,
            stream_ids: DomRefCell::new(stream_ids),
            closed: Cell::new(false),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        track: Option<&MediaStreamTrack>,
        controller: WebRtcController,
        id: TransceiverId,
        kind: TransceiverKind,
        stream_ids: Vec<String>,
    ) -> DomRoot<Self> {
        reflect_dom_object_with_cx(
            Box::new(Self::new_inherited(track, controller, id, kind, stream_ids)),
            global,
            cx,
        )
    }

    pub(crate) fn close(&self) {
        self.closed.set(true);
    }

    pub(crate) fn detach_track(&self) -> Result<(), String> {
        self.controller
            .replace_track(self.id, None)
            .map_err(|error| format!("{error:?}"))?;
        self.track.set(None);
        Ok(())
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

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcrtpsender-replacetrack>
    fn ReplaceTrack(
        &self,
        cx: &mut JSContext,
        with_track: Option<&MediaStreamTrack>,
    ) -> RootedPromise {
        if self.closed.get() {
            let promise = Promise::new(cx, &self.global());
            promise.reject_error(cx, Error::InvalidState(None));
            return promise;
        }
        if let Some(track) = with_track {
            let kind_matches = match track.ty() {
                MediaStreamType::Audio => self.kind == TransceiverKind::Audio,
                MediaStreamType::Video => self.kind == TransceiverKind::Video,
            };
            if !kind_matches {
                let promise = Promise::new(cx, &self.global());
                promise.reject_error(
                    cx,
                    Error::InvalidModification(Some(
                        "replacement track kind does not match the sender".into(),
                    )),
                );
                return promise;
            }
            let track = OutgoingTrack {
                source_stream: track.id(),
                stream_ids: self.stream_ids.borrow().clone(),
                track_id: track.Id().to_string(),
                audio_bitrate: None,
                video_bitrate: None,
            };
            if let Err(error) = self.controller.replace_track(self.id, Some(track)) {
                let promise = Promise::new(cx, &self.global());
                promise.reject_error(cx, Error::Operation(Some(format!("{error:?}").into())));
                return promise;
            }
            self.track.set(with_track);
            return Promise::new_resolved(cx, &self.global(), ());
        }
        if let Err(error) = self.controller.replace_track(self.id, None) {
            let promise = Promise::new(cx, &self.global());
            promise.reject_error(cx, Error::Operation(Some(format!("{error:?}").into())));
            return promise;
        }
        self.track.set(None);
        Promise::new_resolved(cx, &self.global(), ())
    }
}
