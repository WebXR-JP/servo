/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct ConstrainRange<T> {
    pub min: Option<T>,
    pub max: Option<T>,
    pub ideal: Option<T>,
}

#[derive(Clone, Debug)]
pub enum ConstrainBool {
    Ideal(bool),
    Exact(bool),
}

/// deviceId 制約の中間表現。仕様の意味を保つ。
///
/// 素の文字列・文字列配列・`ideal` 指定は希望として扱い、
/// 一致するデバイスが無ければ既定デバイスへフォールバックする。
/// `exact` 指定は必須であり、一致するデバイスが無ければ取得失敗とする。
/// `exact` に複数指定があれば、そのいずれかに一致すればよい。
/// 文字列 `"default"` は特別値であり、is-default デバイスを意味する
/// （LiveKit が `{ideal: "default"}` を渡す経路があるため）。
#[derive(Clone, Debug, Default)]
pub struct DeviceIdConstraint {
    pub exact: Option<Vec<String>>,
    pub ideal: Vec<String>,
}

/// getUserMedia 系の取得失敗理由。Overconstrained の内側は制約名（今回は "deviceId"）。
///
/// `GenericCallback` の応答型に載せるため serde を実装する。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaStreamError {
    NotFound,
    NotAllowed,
    NotReadable(String),
    Overconstrained(String),
    /// このバックエンドでは画面共有を提供しない（Linux の U1 stub など）。
    NotSupported,
}

/// getDisplayMedia のバックエンド引数。音声制約は読み捨て、`audio` フラグだけを渡す。
/// serde は不要（同一プロセス内の呼び出しのみ）。
#[derive(Clone, Debug, Default)]
pub struct DisplayCaptureConstraints {
    pub video: MediaTrackConstraintSet,
    /// false = 音声トラックを返してはならない。
    /// true = 音声を要求するが、バックエンドが音声を提供できない場合は映像のみ
    /// 返してよい（XRift はこの契約で動く）。現状の U2 Linux 実装は映像のみを返す。
    pub audio: bool,
}

#[derive(Clone, Debug, Default)]
pub struct MediaTrackConstraintSet {
    pub width: Option<Constrain<u32>>,
    pub height: Option<Constrain<u32>>,
    pub aspect: Option<Constrain<f64>>,
    pub frame_rate: Option<Constrain<f64>>,
    pub sample_rate: Option<Constrain<u32>>,
    pub device_id: Option<DeviceIdConstraint>,
}

#[derive(Clone, Debug)]
pub enum Constrain<T> {
    /// 必須制約。`{exact}` 指定に対応する。
    /// デバイス選択を絞り、満たせなければ取得失敗とする。
    Value(T),
    /// 必須の範囲制約。`min`/`max` 指定に対応する。
    /// `ideal` 同梱時は範囲内で優先する希望値として扱う。
    Range(ConstrainRange<T>),
    /// 希望制約。素の数値・`{ideal}` 指定に対応する。
    /// Chrome と同様の best-effort であり、満たせなくても取得失敗としない。
    /// backend のデバイス選択では絞り込みに使わない。
    Ideal(T),
}
