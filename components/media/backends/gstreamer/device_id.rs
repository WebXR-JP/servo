/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! enumerateDevices と getUserMedia の選択で使う公開デバイス ID の導出。
//!
//! GStreamer のデバイス固有キー（`unique-id` プロパティ。無ければ表示名）
//! から SHA-256 ハッシュの公開 ID を作る。生の `unique-id`
//!（USB のシリアル番号相当を含むことがある）を Web へ返さない。
//!
//! 制限: `unique-id` が無く同名のデバイスが複数ある場合は衝突し得る。
//! 無理に解決せず、衝突時は同一 ID として扱う（選択は先勝ち）。

use gstreamer::prelude::*;

/// 公開 ID の接頭辞。生 ID との見分けと `"default"` 特別値との衝突回避用。
pub const PUBLIC_ID_PREFIX: &str = "xrift-";

/// GStreamer デバイスの内部キー。`unique-id` があればそれを使い、
/// 無ければ表示名にフォールバックする。
pub fn internal_key_for(device: &gstreamer::Device) -> String {
    device
        .properties()
        .and_then(|properties| properties.get::<String>("unique-id").ok())
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| device.display_name().as_str().to_owned())
}

/// GStreamer デバイスが is-default かどうか。
pub fn is_default_device(device: &gstreamer::Device) -> bool {
    device
        .properties()
        .and_then(|properties| properties.get::<bool>("is-default").ok())
        .unwrap_or(false)
}

/// 内部キーから公開 ID（opaque・安定）を導出する。
pub fn public_device_id(internal_key: &str) -> String {
    // GStreamer 1.28.8 の unique-id 例:
    // PicoVirtualMicrophone_UID、BuiltInMicrophoneDevice、
    // AppleUSBAudioEngine:ZOOM Corporation:AMS-24:1132000:1,2。
    let salted = format!("xrift-device:{internal_key}");
    let digest = glib::Checksum::new(glib::ChecksumType::Sha256).map(|mut checksum| {
        checksum.update(salted.as_bytes());
        checksum.string().unwrap_or_default()
    });
    format!("{PUBLIC_ID_PREFIX}{}", digest.unwrap_or_default())
}

/// GStreamer デバイスの公開 ID。
pub fn public_device_id_for(device: &gstreamer::Device) -> String {
    public_device_id(&internal_key_for(device))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_internal_key_gives_same_public_id() {
        let first = public_device_id("PicoVirtualMicrophone_UID");
        let second = public_device_id("PicoVirtualMicrophone_UID");
        assert_eq!(first, second, "同じ入力からは常に同じ ID を返す");
        assert!(
            first.starts_with(PUBLIC_ID_PREFIX),
            "公開 ID は接頭辞で生 ID と区別できる"
        );
    }

    #[test]
    fn different_internal_keys_give_different_public_ids() {
        let pico = public_device_id("PicoVirtualMicrophone_UID");
        let builtin = public_device_id("BuiltInMicrophoneDevice");
        let zoom = public_device_id("AppleUSBAudioEngine:ZOOM Corporation:AMS-24:1132000:1,2");
        assert_ne!(pico, builtin);
        assert_ne!(pico, zoom);
        assert_ne!(builtin, zoom);
    }

    #[test]
    fn public_id_does_not_contain_raw_key() {
        let raw = "AppleUSBAudioEngine:ZOOM Corporation:AMS-24:1132000:1,2";
        let public = public_device_id(raw);
        assert!(
            !public.contains("AMS-24"),
            "公開 ID に生の unique-id を含めない"
        );
        assert!(
            !public.contains("AppleUSBAudioEngine"),
            "公開 ID に生の unique-id を含めない"
        );
    }
}
