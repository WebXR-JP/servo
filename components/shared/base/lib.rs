/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![deny(unsafe_code)]

//! A crate to hold very common types in Servo.
//!
//! You should almost never need to add a data type to this crate. Instead look for
//! a more shared crate that has fewer dependents.

pub mod cross_process_instant;
pub mod generic_channel;
#[macro_use]
pub mod id;
pub mod print_tree;
mod rope;
pub mod text;
pub mod threadboost;
pub mod threadpool;
mod unicode_block;

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;

use log::{trace, warn};
use malloc_size_of_derive::MallocSizeOf;
pub use rope::{Rope, RopeChars, RopeIndex, RopeMovement, RopeSlice};
use serde::{Deserialize, Serialize};
use webrender_api::Epoch as WebRenderEpoch;

pub fn read_json_from_file<T>(data: &mut T, config_dir: &Path, filename: &str)
where
    T: for<'de> Deserialize<'de>,
{
    let path = config_dir.join(filename);
    let display = path.display();

    let mut file = match File::open(&path) {
        Err(why) => {
            warn!("couldn't open {}: {}", display, why);
            return;
        },
        Ok(file) => file,
    };

    let mut string_buffer: String = String::new();
    match file.read_to_string(&mut string_buffer) {
        Err(why) => panic!("couldn't read from {}: {}", display, why),
        Ok(_) => trace!("successfully read from {}", display),
    }

    match serde_json::from_str(&string_buffer) {
        Ok(decoded_buffer) => *data = decoded_buffer,
        Err(why) => warn!("Could not decode buffer{}", why),
    }
}

/// JSON ファイルへデータを書き込む。失敗時はエラーを返す。
///
/// 終了時の一括保存ではなく、変更のたびに保存する処理から呼ぶため、
/// 失敗しても panic せず呼び出し側で扱う。
/// 一時ファイルへ書いてから rename で本ファイルへ置き換える。
/// 本ファイルへ直接書くと書き込み中の強制終了で空や途中で切れた JSON が残り、
/// 次回起動時に読み込めず内容を失うためである。
///
/// # Errors
///
/// 一時ファイルの作成や書き込み、シリアライズ、本ファイルへの置き換えに失敗した場合にエラーを返す。
pub fn try_write_json_to_file<T>(data: &T, config_dir: &Path, filename: &str) -> std::io::Result<()>
where
    T: Serialize,
{
    let path = config_dir.join(filename);
    let display = path.display();
    let temp_path = config_dir.join(format!("{filename}.{}.tmp", std::process::id()));

    {
        let file = match File::create(&temp_path) {
            Ok(file) => file,
            Err(error) => {
                let _ = std::fs::remove_file(&temp_path);
                return Err(error);
            },
        };
        let mut writer = BufWriter::new(file);
        if let Err(error) =
            serde_json::to_writer_pretty(&mut writer, data).map_err(std::io::Error::other)
        {
            let _ = std::fs::remove_file(&temp_path);
            return Err(error);
        }
        if let Err(error) = writer.flush().map_err(std::io::Error::other) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(error);
        }
    }
    if let Err(error) = std::fs::rename(&temp_path, &path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    trace!("successfully wrote to {display}");
    Ok(())
}

/// JSON ファイルへデータを書き込む。
///
/// 書き込みに失敗した場合は異常終了させるため、
/// 失敗を見過ごせない箇所（終了時の一括保存など）でのみ使う。変更のたびの保存には
/// [`try_write_json_to_file`] を使い、失敗を警告に留める。
///
/// # Panics
///
/// ファイルの作成やシリアライズに失敗した場合にパニックする。
pub fn write_json_to_file<T>(data: &T, config_dir: &Path, filename: &str)
where
    T: Serialize,
{
    if let Err(why) = try_write_json_to_file(data, config_dir, filename) {
        panic!(
            "couldn't write to {}: {}",
            config_dir.join(filename).display(),
            why
        );
    }
}

/// A struct for denoting the age of messages; prevents race conditions.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Deserialize,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    MallocSizeOf,
)]
pub struct Epoch(pub u32);

impl Epoch {
    pub fn next(&self) -> Self {
        Self(self.0 + 1)
    }
}

impl From<Epoch> for WebRenderEpoch {
    fn from(val: Epoch) -> Self {
        WebRenderEpoch(val.0)
    }
}

pub trait WebRenderEpochToU16 {
    fn as_u16(&self) -> u16;
}

impl WebRenderEpochToU16 for WebRenderEpoch {
    /// The value of this [`Epoch`] as a u16 value. Note that if this Epoch's
    /// value is more than u16::MAX, then the return value will be modulo
    /// u16::MAX.
    fn as_u16(&self) -> u16 {
        (self.0 % u16::MAX as u32) as u16
    }
}
