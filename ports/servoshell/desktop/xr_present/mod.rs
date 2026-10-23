//! `--xr-present-test`モードのOpenXR提示PoC。
//!
//! 同一プロセス内の専用スレッドでOpenXRセッション(D3D11)を作り、生成した
//! カラーパターンを`CompositionLayerQuad` 1枚で提示する。Servoのページ画像取得は
//! 次の作業単位のため対象外であり、メインスレッド(winit/Servo)とは結合しない
//! (画像の受け渡しは行わず、メイン側で`xrWaitFrame`等の待ちも行わない)。
//!
//! ADR: ページ画像の受け渡しが必要になったら「最新1枚・上書き可・待たせない」
//! スロット(TripleBuffer相当)を本モジュールとServo側の間に足す。現状はXR
//! スレッドが自前のパターンバッファを所有し、他スレッドと共有状態を持たない。
//! ADR: `--seconds`期限の到達時はXRスレッドから`process::exit`でプロセス全体を
//! 終了させる。winitイベントループ側の正常シャットダウン経路は使わないPoC判断。

use crate::prefs::ServoShellPreferences;

#[cfg(target_os = "windows")]
mod imp;

#[cfg(not(target_os = "windows"))]
pub(crate) fn spawn(_preferences: &ServoShellPreferences) {
    eprintln!("error: --xr-present-test is only supported on Windows (D3D11)");
    std::process::exit(2);
}

#[cfg(target_os = "windows")]
pub(crate) fn spawn(preferences: &ServoShellPreferences) {
    let seconds = preferences.xr_present_seconds;
    std::thread::Builder::new()
        .name("xr-present".into())
        // 初期化失敗はその場で落とす PoC 判断。フレームループ内では`unwrap`しない。
        .spawn(move || imp::run(seconds))
        // 想定条件: スレッド生成に失敗するのはリソース枯渇など回復不能な場合のみ。
        .expect("failed to spawn xr-present thread");
}
