/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};

use dom_struct::dom_struct;
use embedder_traits::{AllowOrDeny, EmbedderMsg};
use js::context::JSContext;
use js::realm::CurrentRealm;
use script_bindings::num::Finite;
use script_bindings::reflector::reflect_dom_object_with_cx;
use servo_media::ServoMedia;
use servo_media::streams::MediaStreamType;
use servo_media::streams::capture::{
    Constrain, ConstrainRange, DeviceIdConstraint, MediaStreamError, MediaTrackConstraintSet,
};
use servo_media::streams::device_monitor::MediaDeviceKind as ServoMediaDeviceKind;
use servo_media::streams::registry::stop_stream;

use crate::conversions::Convert;
use crate::dom::bindings::codegen::Bindings::MediaDevicesBinding::{
    ConstrainDOMStringParameters, ConstrainDoubleRange, ConstrainULongRange, DoubleRange,
    MediaDevicesMethods, MediaStreamConstraints, MediaTrackConstraints,
    MediaTrackConstraintSet as JsConstraintSet, ULongRange,
};
use crate::dom::bindings::codegen::UnionTypes::{
    BooleanOrMediaTrackConstraints, ClampedUnsignedLongOrConstrainULongRange as ConstrainULong,
    DoubleOrConstrainDoubleRange as ConstrainDouble, StringOrStringSequence,
    StringOrStringSequenceOrConstrainDOMStringParameters as ConstrainDOMString,
};
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::bindings::error::Error;
use crate::dom::bindings::refcounted::{Trusted, TrustedPromise};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::document::Document;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::media::mediadeviceinfo::MediaDeviceInfo;
use crate::dom::media::mediastream::MediaStream;
use crate::dom::media::mediastreamtrack::MediaStreamTrack;
use crate::dom::promise::{Promise, RootedPromise};
use crate::routed_promise::{RoutedPromiseListener, callback_promise};

/// マイク権限の document ごとの記録。OS の TCC が正であり、永続化しない。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum MicPermissionState {
    /// まだ問い合わせていない。次の音声要求で embedder へ確認する。
    #[default]
    Unknown,
    /// 許可済み。以降の音声要求は即時にストリームを作る。
    Granted,
    /// 拒否済み。以降の音声要求は即時に `NotAllowedError` で拒否する。
    Denied,
}

/// 権限応答待ちの getUserMedia 要求1件の所有データ。
/// 生成辞書（`MediaTrackConstraints`）は Clone 不可・JSTraceable のため、
/// JS 値を保持しない servo-media 側の制約集合と辞書指定の有無だけを残す。
/// track 生成時に等価な辞書を再構築する（`getConstraints()` の完全な
/// 忠実性は目的としない。`rebuild_constraints` を参照）。
struct PendingMicRequest {
    /// 音声制約と辞書指定の有無。偽は `Boolean(true)` に対応する。
    audio: Option<(MediaTrackConstraintSet, bool)>,
    /// 映像制約と辞書指定の有無。カメラ権限は今回扱わないが、要求全体を
    /// 待たせるため一緒に保持する。
    video: Option<(MediaTrackConstraintSet, bool)>,
    /// 要求元の document。応答時に非アクティブならその要求だけ拒否し、
    /// 古い document で新しいマイクを開かない。
    document: Trusted<Document>,
    /// 解決・拒否する promise。GC 保護のため保持表に pin する。
    promise: TrustedPromise,
}

/// 応答待ちの要求群。`TrustedPromise` は promise 保持表で GC 保護するため
/// trace 対象外とする。借用中に script を実行しないため通常の `RefCell` で足りる。
#[derive(Default)]
struct PendingGetUserMedia(RefCell<Vec<PendingMicRequest>>);

impl PendingGetUserMedia {
    fn push(&self, request: PendingMicRequest) {
        self.0.borrow_mut().push(request);
    }

    fn take_all(&self) -> Vec<PendingMicRequest> {
        std::mem::take(&mut *self.0.borrow_mut())
    }
}

#[dom_struct]
pub(crate) struct MediaDevices {
    eventtarget: EventTarget,
    /// マイク権限の記録。実体は `#[no_trace]` で trace を外す。
    #[no_trace]
    #[ignore_malloc_size_of = "Copy できる小さな enum で保持表と無関係"]
    mic_permission: Cell<MicPermissionState>,
    /// enumerate で実 ID・label を出してよいか。権限付与で真になる。
    audio_exposed: Cell<bool>,
    /// 権限要求が embedder への往復中か。同時要求の多重送信を抑える。
    permission_in_flight: Cell<bool>,
    /// 応答待ちの要求群。応答時に同じ判断でまとめて処理する。
    #[no_trace]
    #[ignore_malloc_size_of = "TrustedPromise pins live JS promises outside the heap graph"]
    pending: PendingGetUserMedia,
}

impl MediaDevices {
    pub(crate) fn new_inherited() -> MediaDevices {
        MediaDevices {
            eventtarget: EventTarget::new_inherited(),
            mic_permission: Cell::new(MicPermissionState::Unknown),
            audio_exposed: Cell::new(false),
            permission_in_flight: Cell::new(false),
            pending: PendingGetUserMedia::default(),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<MediaDevices> {
        reflect_dom_object_with_cx(Box::new(MediaDevices::new_inherited()), global, cx)
    }

    /// 音声・映像の制約集合からストリームを作り、promise を解決・拒否する。
    /// 即時パスと権限保留パスの両方から使う。各 `bool` は辞書指定の有無であり、
    /// 偽（`Boolean(true)`）なら空制約の track を作る。
    fn resolve_streams(
        &self,
        cx: &mut JSContext,
        promise: &RootedPromise,
        audio: Option<(MediaTrackConstraintSet, bool)>,
        video: Option<(MediaTrackConstraintSet, bool)>,
    ) {
        let media = ServoMedia::get();
        let mut audio_track = None;
        let mut audio_id = None;
        if let Some((audio_set, is_dict)) = audio {
            // 保留時は生成辞書を保持できないため、track 生成用に等価な辞書を
            // 先に再構築する。backend への選択は元の集合のまま行う。
            let rebuilt = if is_dict {
                Some(rebuild_constraints(&audio_set))
            } else {
                None
            };
            let audio_stream = match media.create_audioinput_stream(audio_set) {
                Ok(stream) => stream,
                Err(error) => {
                    promise.reject_error(cx, convert_stream_error(&error));
                    return;
                },
            };
            audio_id = Some(audio_stream);
            audio_track = Some(match rebuilt {
                Some(rebuilt) => MediaStreamTrack::new_with_constraints(
                    cx,
                    &self.global(),
                    audio_stream,
                    MediaStreamType::Audio,
                    &rebuilt,
                ),
                None => MediaStreamTrack::new(
                    cx,
                    &self.global(),
                    audio_stream,
                    MediaStreamType::Audio,
                ),
            });
        }

        let mut video_track = None;
        if let Some((video_set, is_dict)) = video {
            let rebuilt = if is_dict {
                Some(rebuild_constraints(&video_set))
            } else {
                None
            };
            let video_stream = match media.create_videoinput_stream(video_set) {
                Ok(stream) => stream,
                Err(error) => {
                    if let Some(audio) = audio_id {
                        let _ = stop_stream(&audio);
                    }
                    promise.reject_error(cx, convert_stream_error(&error));
                    return;
                },
            };
            video_track = Some(match rebuilt {
                Some(rebuilt) => MediaStreamTrack::new_with_constraints(
                    cx,
                    &self.global(),
                    video_stream,
                    MediaStreamType::Video,
                    &rebuilt,
                ),
                None => MediaStreamTrack::new(
                    cx,
                    &self.global(),
                    video_stream,
                    MediaStreamType::Video,
                ),
            });
        }

        let stream = MediaStream::new(cx, &self.global());
        if let Some(track) = audio_track {
            stream.add_track(&track);
        }
        if let Some(track) = video_track {
            stream.add_track(&track);
        }

        promise.resolve_native(cx, &stream);
    }
}

impl RoutedPromiseListener<crate::DomTypeHolder, AllowOrDeny> for MediaDevices {
    fn handle_response(
        &self,
        cx: &mut JSContext,
        response: AllowOrDeny,
        _promise: &RootedPromise,
    ) {
        let granted = matches!(response, AllowOrDeny::Allow);
        // 応答は確定記録し、合流した保留をすべて同じ判断で処理する。
        // 同じ document のマイク権限のため、個別の可否は付けない。
        // 引数の promise は往復用の pin であり、解決は保留側の pin で行う。
        self.mic_permission.set(if granted {
            MicPermissionState::Granted
        } else {
            MicPermissionState::Denied
        });
        if granted {
            self.audio_exposed.set(true);
        }
        self.permission_in_flight.set(false);
        for request in self.pending.take_all() {
            let promise = request.promise.root(cx);
            if !granted {
                promise.reject_error(cx, Error::NotAllowed(None));
                continue;
            }
            if !request.document.root().is_fully_active() {
                // 古い document で新しいマイクを開かない。
                promise.reject_error(
                    cx,
                    Error::NotAllowed(Some(
                        "Requesting document is not fully active.".to_owned(),
                    )),
                );
                continue;
            }
            self.resolve_streams(cx, &promise, request.audio, request.video);
        }
    }
}

impl MediaDevicesMethods<crate::DomTypeHolder> for MediaDevices {
    /// <https://w3c.github.io/mediacapture-main/#dom-mediadevices-getusermedia>
    fn GetUserMedia(
        &self,
        cx: &mut CurrentRealm,
        constraints: &MediaStreamConstraints,
    ) -> RootedPromise {
        let p = Promise::new_in_realm(cx);
        let audio_constraints = convert_constraints(&constraints.audio);
        let video_constraints = convert_constraints(&constraints.video);
        if audio_constraints.is_none() && video_constraints.is_none() {
            p.reject_error(
                cx,
                Error::Type(c"At least one of audio or video must be requested".to_owned()),
            );
            return p;
        }
        let audio_is_dict = matches!(
            constraints.audio,
            BooleanOrMediaTrackConstraints::MediaTrackConstraints(_)
        );
        let video_is_dict = matches!(
            constraints.video,
            BooleanOrMediaTrackConstraints::MediaTrackConstraints(_)
        );

        // 映像だけの要求はマイク権限と無関係に即時生成する。
        if audio_constraints.is_none() {
            self.resolve_streams(
                cx,
                &p,
                None,
                video_constraints.map(|set| (set, video_is_dict)),
            );
            return p;
        }

        match self.mic_permission.get() {
            MicPermissionState::Granted => {
                self.resolve_streams(
                    cx,
                    &p,
                    audio_constraints.map(|set| (set, audio_is_dict)),
                    video_constraints.map(|set| (set, video_is_dict)),
                );
                p
            },
            MicPermissionState::Denied => {
                p.reject_error(cx, Error::NotAllowed(None));
                p
            },
            MicPermissionState::Unknown => {
                let Some(webview_id) = self.global().webview_id() else {
                    p.reject_error(
                        cx,
                        Error::NotAllowed(Some(
                            "Unable to obtain microphone permission.".to_owned(),
                        )),
                    );
                    return p;
                };
                // 所有データと promise を保留し、往復中以外は embedder へ確認する。
                // 同じ document の同時要求は合流させ、権限要求は1回に抑える。
                let global = self.global();
                let document = global.as_window().Document();
                self.pending.push(PendingMicRequest {
                    audio: audio_constraints.map(|set| (set, audio_is_dict)),
                    video: video_constraints.map(|set| (set, video_is_dict)),
                    document: Trusted::new(&document),
                    promise: TrustedPromise::new(&p),
                });
                if !self.permission_in_flight.get() {
                    self.permission_in_flight.set(true);
                    let task_manager = global.task_manager();
                    let task_source =
                        task_manager.dom_manipulation_task_source();
                    let callback = callback_promise(&p, self, task_source);
                    global.send_to_embedder(
                        EmbedderMsg::RequestMicrophonePermission(webview_id, callback),
                    );
                }
                p
            },
        }
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediadevices-enumeratedevices>
    fn EnumerateDevices(&self, cx: &mut JSContext) -> RootedPromise {
        // Step 1.
        let mut realm = CurrentRealm::assert(cx);
        let p = Promise::new_in_realm(&mut realm);

        // Step 2.
        // XXX These steps should be run in parallel.
        // XXX Steps 2.1 - 2.4

        // Step 2.5
        let media = ServoMedia::get();
        let device_monitor = media.get_device_monitor();
        let exposed = self.audio_exposed.get();
        let result_list = device_monitor
            .enumerate_devices()
            .map(|devices| {
                // 権限前は音声入力の実 ID・label を隠す。最大1件だけ空で返し、
                // 残りは落とす。映像は今回マスクしない（カメラ権限は範囲外）。
                let mut audio_hidden = false;
                devices
                    .iter()
                    .filter_map(|device| {
                        if !exposed
                            && matches!(device.kind, ServoMediaDeviceKind::AudioInput)
                        {
                            if audio_hidden {
                                return None;
                            }
                            audio_hidden = true;
                            return Some(MediaDeviceInfo::new(
                                cx,
                                &self.global(),
                                "",
                                device.kind.convert(),
                                "",
                                "",
                            ));
                        }
                        Some(MediaDeviceInfo::new(
                            cx,
                            &self.global(),
                            &device.device_id,
                            device.kind.convert(),
                            &device.label,
                            "",
                        ))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        p.resolve_native(cx, &result_list);

        // Step 3.
        p
    }
}

/// 保留していた servo-media 側の制約集合から、track 生成用の JS 辞書を再構築する。
/// `Value`（exact 由来）・`Ideal`（素の数値・ideal 由来）とも素の数値に寄せる。
/// deviceId は exact・ideal の両方を保つ。`getConstraints()` の完全な忠実性より、
/// backend の選択に使った集合の再現を優先する暫定であり、辞書の往復が
/// 必要になれば所有型を見直す。
fn rebuild_constraints(set: &MediaTrackConstraintSet) -> MediaTrackConstraints {
    MediaTrackConstraints {
        parent: JsConstraintSet {
            width: set.width.as_ref().map(rebuild_ulong),
            height: set.height.as_ref().map(rebuild_ulong),
            aspectRatio: set.aspect.as_ref().and_then(rebuild_double),
            frameRate: set.frame_rate.as_ref().and_then(rebuild_double),
            sampleRate: set.sample_rate.as_ref().map(rebuild_ulong),
            deviceId: set.device_id.as_ref().map(rebuild_device_id),
        },
        advanced: None,
    }
}

fn rebuild_ulong(constraint: &Constrain<u32>) -> ConstrainULong {
    match constraint {
        Constrain::Value(value) => ConstrainULong::ClampedUnsignedLong(*value),
        // 希望値は素の数値に戻す。exact との区別は backend 選択後の再構築では保てない。
        Constrain::Ideal(value) => ConstrainULong::ClampedUnsignedLong(*value),
        Constrain::Range(range) => {
            ConstrainULong::ConstrainULongRange(ConstrainULongRange {
                parent: ULongRange {
                    max: range.max,
                    min: range.min,
                },
                exact: None,
                ideal: range.ideal,
            })
        },
    }
}

fn rebuild_double(constraint: &Constrain<f64>) -> Option<ConstrainDouble> {
    match constraint {
        // 有限でない値は辞書に戻せないため落とす。
        // 希望値は素の数値に戻す（exact との区別は再構築では保てない）。
        Constrain::Value(value) => Finite::new(*value).map(ConstrainDouble::Double),
        Constrain::Ideal(value) => Finite::new(*value).map(ConstrainDouble::Double),
        Constrain::Range(range) => {
            let min = range.min.and_then(Finite::new);
            let max = range.max.and_then(Finite::new);
            let ideal = range.ideal.and_then(Finite::new);
            // すべて落ちたら制約自体が無いものとして扱う。
            if min.is_none() && max.is_none() && ideal.is_none() {
                return None;
            }
            Some(ConstrainDouble::ConstrainDoubleRange(
                ConstrainDoubleRange {
                    parent: DoubleRange { max, min },
                    exact: None,
                    ideal,
                },
            ))
        },
    }
}

fn rebuild_device_id(constraint: &DeviceIdConstraint) -> ConstrainDOMString {
    let exact = constraint
        .exact
        .as_ref()
        .map(|ids| string_or_sequence(ids));
    let ideal = if constraint.ideal.is_empty() {
        None
    } else {
        Some(string_or_sequence(&constraint.ideal))
    };
    // 素の文字列・配列は希望（ideal）として扱った変換と対称にするため、
    // 単独の ideal 1件は素の文字列に戻す。
    if exact.is_none() {
        match ideal {
            Some(StringOrStringSequence::String(single)) => {
                return ConstrainDOMString::String(single);
            },
            Some(StringOrStringSequence::StringSequence(ids)) if ids.len() == 1 => {
                return ConstrainDOMString::String(ids.into_iter().next().expect(
                    "長さ1の配列から1件が取れるはず",
                ));
            },
            Some(StringOrStringSequence::StringSequence(ids)) => {
                return ConstrainDOMString::StringSequence(ids);
            },
            _ => {},
        }
    }
    ConstrainDOMString::ConstrainDOMStringParameters(ConstrainDOMStringParameters {
        exact,
        ideal,
    })
}

/// 複数 ID は配列、1件は素の文字列にする。空配列は呼ばれない前提。
fn string_or_sequence(ids: &[String]) -> StringOrStringSequence {
    if ids.len() == 1 {
        StringOrStringSequence::String(ids[0].clone().into())
    } else {
        StringOrStringSequence::StringSequence(
            ids.iter().map(|id| id.clone().into()).collect(),
        )
    }
}

fn convert_constraints(js: &BooleanOrMediaTrackConstraints) -> Option<MediaTrackConstraintSet> {
    match js {
        BooleanOrMediaTrackConstraints::Boolean(false) => None,
        BooleanOrMediaTrackConstraints::Boolean(true) => Some(Default::default()),
        BooleanOrMediaTrackConstraints::MediaTrackConstraints(c) => Some(MediaTrackConstraintSet {
            height: c.parent.height.as_ref().and_then(convert_culong),
            width: c.parent.width.as_ref().and_then(convert_culong),
            aspect: c.parent.aspectRatio.as_ref().and_then(convert_cdouble),
            frame_rate: c.parent.frameRate.as_ref().and_then(convert_cdouble),
            sample_rate: c.parent.sampleRate.as_ref().and_then(convert_culong),
            device_id: c.parent.deviceId.as_ref().and_then(convert_device_id),
        }),
    }
}

/// Backend の [`MediaStreamError`] を DOM 例外へ変換する。
/// OverconstrainedError インターフェイスは未実装のため、今回は
/// `NotFound("overconstrained: ...")` にフォールバックする（TODO）。
fn convert_stream_error(error: &MediaStreamError) -> Error {
    match error {
        MediaStreamError::NotFound => Error::NotFound(None),
        MediaStreamError::NotAllowed => Error::NotAllowed(None),
        MediaStreamError::NotReadable(message) => Error::NotReadable(Some(message.clone())),
        MediaStreamError::Overconstrained(name) => {
            Error::NotFound(Some(format!("overconstrained: {name}")))
        },
    }
}

/// 素の文字列・文字列配列は希望（ideal）として扱う。
fn convert_string_or_sequence(js: &StringOrStringSequence) -> Vec<String> {
    match js {
        StringOrStringSequence::String(value) => vec![value.to_string()],
        StringOrStringSequence::StringSequence(values) => {
            values.iter().map(|value| value.to_string()).collect()
        },
    }
}

fn convert_device_id(js: &ConstrainDOMString) -> Option<DeviceIdConstraint> {
    match js {
        ConstrainDOMString::String(value) => Some(DeviceIdConstraint {
            exact: None,
            ideal: vec![value.to_string()],
        }),
        ConstrainDOMString::StringSequence(values) => Some(DeviceIdConstraint {
            exact: None,
            ideal: values.iter().map(|value| value.to_string()).collect(),
        }),
        ConstrainDOMString::ConstrainDOMStringParameters(params) => {
            Some(device_id_from_params(params))
        },
    }
}

fn device_id_from_params(params: &ConstrainDOMStringParameters) -> DeviceIdConstraint {
    DeviceIdConstraint {
        exact: params.exact.as_ref().map(convert_string_or_sequence),
        ideal: params
            .ideal
            .as_ref()
            .map(convert_string_or_sequence)
            .unwrap_or_default(),
    }
}

/// 素の数値・`{ideal}` は希望（best-effort）、`{exact}` のみ必須として扱う。
/// Chrome と同様に、希望値はデバイス選択を絞らず、満たせなくても失敗としない。
fn convert_culong(js: &ConstrainULong) -> Option<Constrain<u32>> {
    match js {
        ConstrainULong::ClampedUnsignedLong(val) => Some(Constrain::Ideal(*val)),
        ConstrainULong::ConstrainULongRange(range) => {
            // exact があれば必須を優先する。min/max との同時指定時の競合回避。
            if let Some(exact) = range.exact {
                return Some(Constrain::Value(exact));
            }
            if range.parent.min.is_some() || range.parent.max.is_some() {
                Some(Constrain::Range(ConstrainRange {
                    min: range.parent.min,
                    max: range.parent.max,
                    ideal: range.ideal,
                }))
            } else {
                range.ideal.map(Constrain::Ideal)
            }
        },
    }
}

/// 素の数値・`{ideal}` は希望（best-effort）、`{exact}` のみ必須として扱う。
fn convert_cdouble(js: &ConstrainDouble) -> Option<Constrain<f64>> {
    match js {
        ConstrainDouble::Double(val) => Some(Constrain::Ideal(**val)),
        ConstrainDouble::ConstrainDoubleRange(range) => {
            // exact があれば必須を優先する。min/max との同時指定時の競合回避。
            if let Some(exact) = range.exact {
                return Some(Constrain::Value(*exact));
            }
            if range.parent.min.is_some() || range.parent.max.is_some() {
                Some(Constrain::Range(ConstrainRange {
                    min: range.parent.min.map(|x| *x),
                    max: range.parent.max.map(|x| *x),
                    ideal: range.ideal.map(|x| *x),
                }))
            } else {
                range.ideal.map(|ideal| Constrain::Ideal(*ideal))
            }
        },
    }
}
