/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

pub mod capture;
pub mod device_monitor;
pub mod registry;

use std::any::Any;

pub use registry::*;

pub trait MediaStream: Any + Send {
    fn as_any(&self) -> &dyn Any;
    fn as_mut_any(&mut self) -> &mut dyn Any;
    fn set_id(&mut self, id: registry::MediaStreamId);
    fn ty(&self) -> MediaStreamType;
    /// このトラックの出力枝だけを有効・無効にする。
    fn set_enabled(&mut self, _: bool) -> Result<(), String> {
        Err(String::from(
            "このメディアストリームは有効状態を変更できない",
        ))
    }
    /// このトラックの出力枝を複製する。
    fn clone_stream(&mut self) -> Result<registry::MediaStreamId, String> {
        Err(String::from("このメディアストリームは複製できない"))
    }
    /// このトラックの出力枝を終了する。
    fn stop(&mut self) -> Result<(), String> {
        Err(String::from("このメディアストリームは終了できない"))
    }
    fn settings(&self) -> MediaStreamSettings {
        MediaStreamSettings::default()
    }
}

/// メディア入力の確定済み設定。未知または未確定の値は `None` とする。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MediaStreamSettings {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub aspect_ratio: Option<f64>,
    pub frame_rate: Option<f64>,
    pub sample_rate: Option<u32>,
}

/// A MediaSocket is a way for a backend to represent a
/// yet-to-be-connected source side of a MediaStream
pub trait MediaSocket: Any + Send {
    fn as_any(&self) -> &dyn Any;
}

/// This isn't part of the webrtc spec; it's a leaky abstaction while media streams
/// are under development and example consumers need to be able to inspect them.
pub trait MediaOutput: Send {
    fn add_stream(&mut self, stream: &registry::MediaStreamId);
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MediaStreamType {
    Video,
    Audio,
}
