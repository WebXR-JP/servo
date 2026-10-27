/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use script_bindings::num::Finite;
use script_bindings::reflector::reflect_dom_object;
use servo_media::streams::registry::{
    MediaStreamId, clone_stream, set_stream_enabled, stop_stream, stream_settings,
};
use servo_media::streams::MediaStreamType;

use crate::dom::bindings::codegen::Bindings::MediaDevicesBinding::{
    ConstrainDoubleRange, ConstrainULongRange, DoubleRange, MediaTrackConstraintSet,
    MediaTrackConstraints, ULongRange,
};
use crate::dom::bindings::codegen::Bindings::MediaStreamTrackBinding::{
    MediaStreamTrackMethods, MediaStreamTrackState, MediaTrackSettings,
};
use crate::dom::bindings::codegen::UnionTypes::{
    ClampedUnsignedLongOrConstrainULongRange as ConstrainULong,
    DoubleOrConstrainDoubleRange as ConstrainDouble,
};
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;

#[dom_struct]
pub(crate) struct MediaStreamTrack {
    eventtarget: EventTarget,
    #[ignore_malloc_size_of = "defined in servo-media"]
    #[no_trace]
    id: MediaStreamId,
    web_id: DOMString,
    #[ignore_malloc_size_of = "defined in servo-media"]
    #[no_trace]
    ty: MediaStreamType,
    #[ignore_malloc_size_of = "generated binding dictionary"]
    constraints: MediaTrackConstraints,
    enabled: Cell<bool>,
    ended: Cell<bool>,
}

impl MediaStreamTrack {
    pub(crate) fn new_inherited(
        id: MediaStreamId,
        ty: MediaStreamType,
        web_id: DOMString,
        constraints: MediaTrackConstraints,
    ) -> MediaStreamTrack {
        MediaStreamTrack {
            eventtarget: EventTarget::new_inherited(),
            id,
            web_id,
            ty,
            constraints,
            enabled: Cell::new(true),
            ended: Cell::new(false),
        }
    }

    pub(crate) fn new(
        cx: &mut js::context::JSContext,
        global: &GlobalScope,
        id: MediaStreamId,
        ty: MediaStreamType,
    ) -> DomRoot<MediaStreamTrack> {
        let web_id = id.id().to_string().into();
        Self::new_with_state(
            cx,
            global,
            id,
            ty,
            web_id,
            MediaTrackConstraints::empty(),
            false,
        )
    }

    pub(crate) fn new_with_constraints(
        cx: &mut js::context::JSContext,
        global: &GlobalScope,
        id: MediaStreamId,
        ty: MediaStreamType,
        constraints: &MediaTrackConstraints,
    ) -> DomRoot<MediaStreamTrack> {
        Self::new_with_state(
            cx,
            global,
            id,
            ty,
            id.id().to_string().into(),
            copy_applied_constraints(constraints),
            false,
        )
    }

    pub(crate) fn new_with_id(
        cx: &mut js::context::JSContext,
        global: &GlobalScope,
        id: MediaStreamId,
        ty: MediaStreamType,
        web_id: DOMString,
    ) -> DomRoot<MediaStreamTrack> {
        Self::new_with_state(
            cx,
            global,
            id,
            ty,
            web_id,
            MediaTrackConstraints::empty(),
            false,
        )
    }

    fn new_with_state(
        cx: &mut js::context::JSContext,
        global: &GlobalScope,
        id: MediaStreamId,
        ty: MediaStreamType,
        web_id: DOMString,
        constraints: MediaTrackConstraints,
        ended: bool,
    ) -> DomRoot<MediaStreamTrack> {
        let track = reflect_dom_object(
            cx,
            Box::new(MediaStreamTrack::new_inherited(id, ty, web_id, constraints)),
            global,
        );
        track.ended.set(ended);
        track
    }

    pub(crate) fn id(&self) -> MediaStreamId {
        self.id
    }

    pub(crate) fn ty(&self) -> MediaStreamType {
        self.ty
    }
}

impl MediaStreamTrackMethods<crate::DomTypeHolder> for MediaStreamTrack {
    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-kind>
    fn Kind(&self) -> DOMString {
        match self.ty {
            MediaStreamType::Video => DOMString::from_static("video"),
            MediaStreamType::Audio => DOMString::from_static("audio"),
        }
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-id>
    fn Id(&self) -> DOMString {
        self.web_id.clone()
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-clone>
    fn Clone(&self, cx: &mut js::context::JSContext) -> Fallible<DomRoot<MediaStreamTrack>> {
        if self.ended.get() {
            let clone = MediaStreamTrack::new_with_state(
                cx,
                &self.global(),
                MediaStreamId::new(),
                self.ty,
                uuid::Uuid::new_v4().to_string().into(),
                copy_applied_constraints(&self.constraints),
                true,
            );
            clone.enabled.set(self.enabled.get());
            return Ok(clone);
        }
        let id = clone_stream(&self.id).map_err(|error| Error::Operation(Some(error.into())))?;
        if !self.enabled.get()
            && let Err(error) = set_stream_enabled(&id, false)
        {
            let _ = stop_stream(&id);
            return Err(Error::Operation(Some(error.into())));
        }
        let clone = MediaStreamTrack::new_with_constraints(
            cx,
            &self.global(),
            id,
            self.ty,
            &self.constraints,
        );
        clone.enabled.set(self.enabled.get());
        Ok(clone)
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-enabled>
    fn Enabled(&self) -> bool {
        self.enabled.get()
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-enabled>
    fn SetEnabled(&self, enabled: bool) -> Fallible<()> {
        if self.ended.get() {
            self.enabled.set(enabled);
            return Ok(());
        }
        set_stream_enabled(&self.id, enabled)
            .map_err(|error| Error::Operation(Some(error.into())))?;
        self.enabled.set(enabled);
        Ok(())
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-readystate>
    fn ReadyState(&self) -> MediaStreamTrackState {
        if self.ended.get() {
            MediaStreamTrackState::Ended
        } else {
            MediaStreamTrackState::Live
        }
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-getconstraints>
    fn GetConstraints(&self) -> MediaTrackConstraints {
        copy_applied_constraints(&self.constraints)
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-getsettings>
    fn GetSettings(&self) -> MediaTrackSettings {
        let settings = stream_settings(&self.id).unwrap_or_default();
        MediaTrackSettings {
            width: settings.width,
            height: settings.height,
            aspectRatio: settings.aspect_ratio.and_then(Finite::new),
            frameRate: settings.frame_rate.and_then(Finite::new),
            sampleRate: settings.sample_rate,
        }
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-stop>
    fn Stop(&self) -> Fallible<()> {
        if self.ended.get() {
            return Ok(());
        }
        stop_stream(&self.id).map_err(|error| Error::Operation(Some(error.into())))?;
        self.ended.set(true);
        Ok(())
    }
}

fn copy_applied_constraints(constraints: &MediaTrackConstraints) -> MediaTrackConstraints {
    MediaTrackConstraints {
        parent: copy_constraint_set(&constraints.parent),
        // 現在の capture 変換は base set の hard constraint だけを backend に渡す。
        advanced: None,
    }
}

fn copy_constraint_set(constraints: &MediaTrackConstraintSet) -> MediaTrackConstraintSet {
    MediaTrackConstraintSet {
        aspectRatio: constraints
            .aspectRatio
            .as_ref()
            .and_then(copy_double_constraint),
        frameRate: constraints
            .frameRate
            .as_ref()
            .and_then(copy_double_constraint),
        height: constraints.height.as_ref().and_then(copy_ulong_constraint),
        sampleRate: constraints
            .sampleRate
            .as_ref()
            .and_then(copy_ulong_constraint),
        width: constraints.width.as_ref().and_then(copy_ulong_constraint),
    }
}

fn copy_ulong_constraint(value: &ConstrainULong) -> Option<ConstrainULong> {
    match value {
        ConstrainULong::ClampedUnsignedLong(value) => {
            Some(ConstrainULong::ClampedUnsignedLong(*value))
        },
        ConstrainULong::ConstrainULongRange(value)
            if value.parent.min.is_some() || value.parent.max.is_some() =>
        {
            Some(ConstrainULong::ConstrainULongRange(ConstrainULongRange {
                parent: ULongRange {
                    max: value.parent.max,
                    min: value.parent.min,
                },
                exact: None,
                ideal: None,
            }))
        },
        ConstrainULong::ConstrainULongRange(value) => value.exact.map(|exact| {
            ConstrainULong::ConstrainULongRange(ConstrainULongRange {
                parent: ULongRange {
                    max: None,
                    min: None,
                },
                exact: Some(exact),
                ideal: None,
            })
        }),
    }
}

fn copy_double_constraint(value: &ConstrainDouble) -> Option<ConstrainDouble> {
    match value {
        ConstrainDouble::Double(value) => Some(ConstrainDouble::Double(*value)),
        ConstrainDouble::ConstrainDoubleRange(value)
            if value.parent.min.is_some() || value.parent.max.is_some() =>
        {
            Some(ConstrainDouble::ConstrainDoubleRange(
                ConstrainDoubleRange {
                    parent: DoubleRange {
                        max: value.parent.max,
                        min: value.parent.min,
                    },
                    exact: None,
                    ideal: None,
                },
            ))
        },
        ConstrainDouble::ConstrainDoubleRange(value) => value.exact.map(|exact| {
            ConstrainDouble::ConstrainDoubleRange(ConstrainDoubleRange {
                parent: DoubleRange {
                    max: None,
                    min: None,
                },
                exact: Some(exact),
                ideal: None,
            })
        }),
    }
}
