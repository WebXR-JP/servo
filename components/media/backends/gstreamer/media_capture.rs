/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use gstreamer;
use gstreamer::caps::NoFeature;
use gstreamer::prelude::*;
use log::info;
use servo_media_streams::MediaStreamType;
use servo_media_streams::capture::*;
use servo_media_streams::registry::MediaStreamId;

use super::device_id::{is_default_device, public_device_id_for};
use crate::media_stream::GStreamerMediaStream;

trait AddToCaps {
    type Bound;
    fn add_to_caps(
        &self,
        name: &str,
        min: Self::Bound,
        max: Self::Bound,
        builder: gstreamer::caps::Builder<NoFeature>,
    ) -> Option<gstreamer::caps::Builder<NoFeature>>;
}

impl AddToCaps for Constrain<u32> {
    type Bound = u32;
    fn add_to_caps(
        &self,
        name: &str,
        min: u32,
        max: u32,
        builder: gstreamer::caps::Builder<NoFeature>,
    ) -> Option<gstreamer::caps::Builder<NoFeature>> {
        match self {
            Constrain::Value(v) => Some(builder.field(name, into_i32(*v))),
            // 希望値（素の数値・`{ideal}` 由来）は best-effort であり、
            // caps の固定フィールドにしない。デバイス選択を絞らないことで、
            // 非対応デバイス（例: 48kHz 非対応の BT ヘッドセット）を除外しない。
            // `Value`（`{exact}` 由来）のみ必須として絞る。
            Constrain::Ideal(_) => Some(builder),
            Constrain::Range(r) => {
                let min = into_i32(r.min.unwrap_or(min));
                let max = into_i32(r.max.unwrap_or(max));
                let range = gstreamer::IntRange::<i32>::new(min, max);

                // TODO: Include the ideal caps value in the caps, needs a refactor
                //       of the AddToCaps trait
                Some(builder.field(name, range))
            },
        }
    }
}

fn into_i32(x: u32) -> i32 {
    if x > i32::MAX as u32 {
        i32::MAX
    } else {
        x as i32
    }
}

impl AddToCaps for Constrain<f64> {
    type Bound = i32;
    fn add_to_caps<'a>(
        &self,
        name: &str,
        min: i32,
        max: i32,
        builder: gstreamer::caps::Builder<NoFeature>,
    ) -> Option<gstreamer::caps::Builder<NoFeature>> {
        match self {
            Constrain::Value(v) => {
                Some(builder.field(name, gstreamer::Fraction::approximate_f64(*v)?))
            },
            // 希望値（素の数値・`{ideal}` 由来）は best-effort であり、
            // caps の固定フィールドにしない（u32 側と同様）。
            Constrain::Ideal(_) => Some(builder),
            Constrain::Range(r) => {
                let min = r
                    .min
                    .and_then(gstreamer::Fraction::approximate_f64)
                    .unwrap_or(gstreamer::Fraction::new(min, 1));
                let max = r
                    .max
                    .and_then(gstreamer::Fraction::approximate_f64)
                    .unwrap_or(gstreamer::Fraction::new(max, 1));
                let range = gstreamer::FractionRange::new(min, max);
                // TODO: Include the ideal caps value in the caps, needs a refactor
                //       of the AddToCaps trait
                Some(builder.field(name, range))
            },
        }
    }
}

// TODO(Manishearth): Should support a set of constraints
fn into_caps(set: &MediaTrackConstraintSet, format: &str) -> Option<gstreamer::Caps> {
    let mut builder = gstreamer::Caps::builder(format);
    if let Some(w) = set.width.as_ref() {
        builder = w.add_to_caps("width", 0, 1000000, builder)?;
    }
    if let Some(h) = set.height.as_ref() {
        builder = h.add_to_caps("height", 0, 1000000, builder)?;
    }
    if let Some(aspect) = set.aspect.as_ref() {
        builder = aspect.add_to_caps("pixel-aspect-ratio", 0, 1000000, builder)?;
    }
    if let Some(fr) = set.frame_rate.as_ref() {
        builder = fr.add_to_caps("framerate", 0, 1000000, builder)?;
    }
    if let Some(sr) = set.sample_rate.as_ref() {
        builder = sr.add_to_caps("rate", 0, 1000000, builder)?;
    }
    Some(builder.build())
}

struct GstMediaDevices {
    monitor: gstreamer::DeviceMonitor,
}

/// `"default"` は is-default デバイスを指す特別値。
fn device_matches(device: &gstreamer::Device, wanted: &str) -> bool {
    if wanted == "default" {
        is_default_device(device)
    } else {
        public_device_id_for(device) == wanted
    }
}

impl GstMediaDevices {
    pub fn new() -> Self {
        Self {
            monitor: gstreamer::DeviceMonitor::new(),
        }
    }

    pub fn get_track(
        &self,
        video: bool,
        constraints: &MediaTrackConstraintSet,
    ) -> Result<GstMediaTrack, MediaStreamError> {
        let (format, filter) = if video {
            ("video/x-raw", "Video/Source")
        } else {
            ("audio/x-raw", "Audio/Source")
        };
        let caps = into_caps(constraints, format).ok_or(MediaStreamError::NotFound)?;
        let f = self.monitor.add_filter(Some(filter), Some(&caps));
        let devices = self.monitor.devices();
        if let Some(f) = f {
            let _ = self.monitor.remove_filter(f);
        }
        if devices.is_empty() {
            return Err(MediaStreamError::NotFound);
        }
        // 選択順: exact（必須。いずれかに一致しなければ Overconstrained）→
        // ideal（希望。一致しなければフォールバック）→ is-default → 先頭。
        // exact の不一致を黙って既定デバイスに置き換えない。
        let selected = if let Some(device_id) = constraints.device_id.as_ref() {
            if let Some(exact) = device_id.exact.as_ref() {
                let mut found = None;
                for wanted in exact {
                    if let Some(device) = devices
                        .iter()
                        .find(|&device| device_matches(device, wanted))
                    {
                        found = Some(device.clone());
                        break;
                    }
                }
                match found {
                    Some(device) => device,
                    None => return Err(MediaStreamError::Overconstrained("deviceId".to_owned())),
                }
            } else {
                let mut found = None;
                for wanted in &device_id.ideal {
                    if let Some(device) = devices
                        .iter()
                        .find(|&device| device_matches(device, wanted))
                    {
                        found = Some(device.clone());
                        break;
                    }
                }
                match found {
                    Some(device) => device,
                    None => default_device(&devices),
                }
            }
        } else {
            default_device(&devices)
        };
        let element = selected
            .create_element(None)
            .map_err(|error| MediaStreamError::NotReadable(error.to_string()))?;
        configure_pipewire_target(&element, &selected)?;
        let element = wrap_pipewire_source(element, video)?;
        Ok(GstMediaTrack {
            element,
            device_id: Some(public_device_id_for(&selected)),
        })
    }
}

/// pipewiresrc を tee へ直結すると、下流の caps が決まる前に接続して
/// "target not found" になる。audioconvert/videoconvert を挟んで
/// raw caps を確定させてから返す。
fn wrap_pipewire_source(
    element: gstreamer::Element,
    video: bool,
) -> Result<gstreamer::Element, MediaStreamError> {
    if !is_pipewiresrc(&element) {
        return Ok(element);
    }
    let bin = gstreamer::Bin::new();
    let convert = gstreamer::ElementFactory::make(if video {
        "videoconvert"
    } else {
        "audioconvert"
    })
    .build()
    .map_err(|error| MediaStreamError::NotReadable(error.to_string()))?;
    bin.add_many([&element, &convert])
        .map_err(|error| MediaStreamError::NotReadable(error.to_string()))?;
    gstreamer::Element::link_many([&element, &convert])
        .map_err(|error| MediaStreamError::NotReadable(error.to_string()))?;
    let src_pad = convert.static_pad("src").ok_or_else(|| {
        MediaStreamError::NotReadable("pipewire 入力の変換器に出力 pad が無い".to_owned())
    })?;
    let ghost = gstreamer::GhostPad::with_target(&src_pad)
        .map_err(|error| MediaStreamError::NotReadable(error.to_string()))?;
    bin.add_pad(&ghost)
        .map_err(|error| MediaStreamError::NotReadable(error.to_string()))?;
    info!(
        "get_track: pipewiresrc を {} で wrap した",
        if video { "videoconvert" } else { "audioconvert" }
    );
    Ok(bin.upcast())
}

fn is_pipewiresrc(element: &gstreamer::Element) -> bool {
    element
        .factory()
        .is_some_and(|factory| factory.name() == "pipewiresrc")
}

/// GStreamer の pipewire プロバイダが返す Audio/Source は `pipewiresrc` で、
/// 接続先（`target-object` か `path`）を設定しないと PAUSED への遷移で
/// "target not found" になる。デバイスの `object.serial` / `object.id` から設定する。
fn configure_pipewire_target(
    element: &gstreamer::Element,
    device: &gstreamer::Device,
) -> Result<(), MediaStreamError> {
    if !is_pipewiresrc(element) {
        return Ok(());
    }

    let properties = device.properties().ok_or_else(|| {
        MediaStreamError::NotReadable("pipewire デバイスのプロパティが無い".to_owned())
    })?;

    if let Some(serial) = property_u64(&properties, "object.serial")
        && element.find_property("target-object").is_some()
    {
        if element.find_property("path").is_some() {
            element.set_property("path", None::<String>);
        }
        element.set_property("target-object", serial.to_string());
        info!(
            "get_track: pipewiresrc に target-object={serial} を設定した (device={}, \
             readback={:?}, element={})",
            device.display_name(),
            element.property::<String>("target-object"),
            element.name()
        );
        return Ok(());
    }

    if let Some(node_id) = property_u64(&properties, "object.id")
        && element.find_property("path").is_some()
    {
        // GstDevice::create_element が target-object を既定で入れるため、
        // path と併用しないよう明示的に外す。
        if element.find_property("target-object").is_some() {
            element.set_property("target-object", None::<String>);
        }
        element.set_property("path", node_id.to_string());
        info!(
            "get_track: pipewiresrc に path={node_id} を設定した (device={}, \
             readback={:?}, element={})",
            device.display_name(),
            element.property::<String>("path"),
            element.name()
        );
        return Ok(());
    }
    Err(MediaStreamError::NotReadable(
        "pipewiresrc の target (object.serial / object.id) を設定できない".to_owned(),
    ))
}

fn property_u64(properties: &gstreamer::Structure, name: &str) -> Option<u64> {
    properties
        .get::<u64>(name)
        .ok()
        .or_else(|| {
            properties
                .get::<i64>(name)
                .ok()
                .and_then(|value| u64::try_from(value).ok())
        })
        .or_else(|| properties.get::<u32>(name).ok().map(u64::from))
        .or_else(|| {
            properties
                .get::<i32>(name)
                .ok()
                .and_then(|value| u64::try_from(value).ok())
        })
        .or_else(|| {
            properties
                .get::<String>(name)
                .ok()
                .and_then(|value| value.parse().ok())
        })
}

fn default_device(devices: &glib::List<gstreamer::Device>) -> gstreamer::Device {
    devices
        .iter()
        .find(|&device| is_default_device(device))
        .or_else(|| devices.front())
        .map(|device| device.clone())
        .expect("デバイス一覧が空でないことは呼び出し側で保証する")
}

pub struct GstMediaTrack {
    element: gstreamer::Element,
    device_id: Option<String>,
}

impl GstMediaTrack {
    pub fn device_id(&self) -> Option<&str> {
        self.device_id.as_deref()
    }
}

fn create_input_stream(
    stream_type: MediaStreamType,
    constraint_set: &MediaTrackConstraintSet,
) -> Result<MediaStreamId, MediaStreamError> {
    let devices = GstMediaDevices::new();
    let track = devices.get_track(stream_type == MediaStreamType::Video, constraint_set)?;
    let device_id = track.device_id().map(|id| id.to_owned());
    let result = match stream_type {
        MediaStreamType::Audio => {
            GStreamerMediaStream::try_create_audio_from_device(track.element, device_id)
        },
        MediaStreamType::Video => {
            GStreamerMediaStream::try_create_video_from_device(track.element, device_id)
        },
    };
    result.map_err(MediaStreamError::NotReadable)
}

pub fn create_audioinput_stream(
    constraint_set: MediaTrackConstraintSet,
) -> Result<MediaStreamId, MediaStreamError> {
    create_input_stream(MediaStreamType::Audio, &constraint_set)
}

pub fn create_videoinput_stream(
    constraint_set: MediaTrackConstraintSet,
) -> Result<MediaStreamId, MediaStreamError> {
    create_input_stream(MediaStreamType::Video, &constraint_set)
}

#[cfg(test)]
mod tests {
    use super::super::device_id::{internal_key_for, public_device_id};
    use super::*;

    #[test]
    fn exact_constraints_use_gstreamer_field_names_and_types() {
        gstreamer::init().unwrap();
        let constraints = MediaTrackConstraintSet {
            sample_rate: Some(Constrain::Value(48_000)),
            frame_rate: Some(Constrain::Value(30.0)),
            ..Default::default()
        };
        let caps = into_caps(&constraints, "audio/x-raw").unwrap();
        let fields = caps.structure(0).unwrap();
        assert_eq!(fields.get::<i32>("rate").unwrap(), 48_000);
        assert_eq!(
            fields.get::<gstreamer::Fraction>("framerate").unwrap(),
            gstreamer::Fraction::new(30, 1)
        );
        assert!(!fields.has_field("name"));
    }

    /// 希望値（素の数値・`{ideal}` 由来）は caps に固定フィールドを足さない。
    /// デバイス選択を絞らないことが目的であり、caps 自体は有効なままにする。
    #[test]
    fn ideal_constraints_do_not_narrow_caps() {
        gstreamer::init().unwrap();
        let constraints = MediaTrackConstraintSet {
            sample_rate: Some(Constrain::Ideal(48_000)),
            frame_rate: Some(Constrain::Ideal(30.0)),
            ..Default::default()
        };
        let caps = into_caps(&constraints, "audio/x-raw").unwrap();
        let fields = caps.structure(0).unwrap();
        assert!(
            !fields.has_field("rate"),
            "希望の sampleRate は rate に固定しない"
        );
        assert!(
            !fields.has_field("framerate"),
            "希望の frameRate は framerate に固定しない"
        );
    }

    /// 必須値（`{exact}` 由来）は引き続き caps に固定する。
    #[test]
    fn exact_sample_rate_still_narrows_caps() {
        gstreamer::init().unwrap();
        let constraints = MediaTrackConstraintSet {
            sample_rate: Some(Constrain::Value(48_000)),
            ..Default::default()
        };
        let caps = into_caps(&constraints, "audio/x-raw").unwrap();
        let fields = caps.structure(0).unwrap();
        assert_eq!(fields.get::<i32>("rate").unwrap(), 48_000);
    }

    /// 実機の Audio/Source を使った選択の検証。デバイスが無い環境では skip する。
    fn first_audio_source() -> Option<(String, bool)> {
        gstreamer::init().unwrap();
        let monitor = gstreamer::DeviceMonitor::new();
        let audio_caps = gstreamer_audio::AudioCapsBuilder::new().build();
        let filter = monitor.add_filter(Some("Audio/Source"), Some(&audio_caps));
        let devices = monitor.devices();
        if let Some(filter) = filter {
            let _ = monitor.remove_filter(filter);
        }
        let device = devices.front()?;
        let public_id = public_device_id(&internal_key_for(&device));
        let is_default = is_default_device(&device);
        Some((public_id, is_default))
    }

    #[test]
    fn get_track_without_constraints_selects_a_device() {
        if first_audio_source().is_none() {
            eprintln!("Audio/Source が無いため skip する");
            return;
        }
        let devices = GstMediaDevices::new();
        let track = devices
            .get_track(false, &MediaTrackConstraintSet::default())
            .expect("制約無しで音声入力を選べない");
        assert!(
            track.device_id().is_some(),
            "選択したデバイスの公開 ID を保持する"
        );
    }

    #[test]
    fn get_track_with_exact_device_id() {
        let Some((public_id, _)) = first_audio_source() else {
            eprintln!("Audio/Source が無いため skip する");
            return;
        };
        let devices = GstMediaDevices::new();
        let constraints = MediaTrackConstraintSet {
            device_id: Some(DeviceIdConstraint {
                exact: Some(vec![public_id.clone()]),
                ideal: vec![],
            }),
            ..Default::default()
        };
        let track = devices
            .get_track(false, &constraints)
            .expect("exact で実在デバイスを選べない");
        assert_eq!(track.device_id(), Some(public_id.as_str()));
    }

    #[test]
    fn get_track_with_unknown_exact_fails() {
        if first_audio_source().is_none() {
            eprintln!("Audio/Source が無いため skip する");
            return;
        }
        let devices = GstMediaDevices::new();
        let constraints = MediaTrackConstraintSet {
            device_id: Some(DeviceIdConstraint {
                exact: Some(vec!["存在しないデバイスID".to_owned()]),
                ideal: vec![],
            }),
            ..Default::default()
        };
        match devices.get_track(false, &constraints) {
            Err(MediaStreamError::Overconstrained(name)) => {
                assert_eq!(name, "deviceId", "失敗した制約名を返す")
            },
            Err(other) => panic!("存在しない exact は Overconstrained になる: {other:?}"),
            Ok(_) => panic!("存在しない exact は黙って既定デバイスに置き換えない"),
        }
    }

    #[test]
    fn get_track_with_unknown_ideal_falls_back() {
        if first_audio_source().is_none() {
            eprintln!("Audio/Source が無いため skip する");
            return;
        }
        let devices = GstMediaDevices::new();
        let constraints = MediaTrackConstraintSet {
            device_id: Some(DeviceIdConstraint {
                exact: None,
                ideal: vec!["存在しないデバイスID".to_owned()],
            }),
            ..Default::default()
        };
        let track = devices
            .get_track(false, &constraints)
            .expect("未知の ideal は既定デバイスへフォールバックする");
        assert!(
            track.device_id().is_some(),
            "フォールバック先デバイスの公開 ID を保持する"
        );
    }
}
