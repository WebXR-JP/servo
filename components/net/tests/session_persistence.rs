/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! `SessionPersistence` の暗号化・移行・異常系の検証。
//!
//! 実の資格情報ストアは使わず、`from_key`・`without_key` で鍵の有無を再現する。
//! 実ストアを使う試験は末尾の ignore 付きテストに隔離する。

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use net::cookie::ServoCookie;
use net::cookie_storage::CookieStorage;
use net::resource_thread::{AuthCache, AuthCacheEntry};
use net::session_persistence::{SessionFile, SessionPersistence};
use net_traits::CookieSource;
use servo_url::ServoUrl;

const TEST_KEY: [u8; 32] = [42; 32];
const SECRET: &str = "supersecret-cookie-value-abc123";

fn cookie_jar_with_secret() -> CookieStorage {
    let url = ServoUrl::parse("https://example.com/").unwrap();
    let mut jar = CookieStorage::new(150);
    let cookie = ServoCookie::from_cookie_string(
        &format!("session={SECRET}; Path=/"),
        &url,
        CookieSource::HTTP,
    )
    .expect("テスト用 Cookie を作れる");
    jar.push(cookie, &url, CookieSource::HTTP);
    jar
}

fn auth_cache_with_secret() -> AuthCache {
    let mut entries = HashMap::new();
    entries.insert(
        "https://example.com".to_owned(),
        AuthCacheEntry {
            user_name: "user".to_owned(),
            password: SECRET.to_owned(),
        },
    );
    AuthCache {
        version: 1,
        entries,
    }
}

fn read_file_bytes(dir: &Path, name: &str) -> Vec<u8> {
    fs::read(dir.join(name)).expect("テスト用ファイルを読める")
}

fn assert_no_temp_files(dir: &Path) {
    for entry in fs::read_dir(dir).expect("一時ディレクトリを読める") {
        let name = entry.expect("項目を読める").file_name();
        let name = name.to_string_lossy();
        assert!(!name.ends_with(".tmp"), "一時ファイルが残っている: {name}",);
    }
}

#[test]
fn encrypted_roundtrip_hides_secret() {
    let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
    let persistence = SessionPersistence::from_key(dir.path().to_path_buf(), TEST_KEY);
    let jar = cookie_jar_with_secret();

    persistence
        .write(SessionFile::Cookies, &jar)
        .expect("暗号化して保存できる");

    let bytes = read_file_bytes(dir.path(), "cookie_jar.json");
    assert!(bytes.starts_with(b"XRIFTSEC"), "暗号化形式で保存する");
    assert!(
        !String::from_utf8_lossy(&bytes).contains(SECRET),
        "平文の秘密を含まない",
    );
    assert_no_temp_files(dir.path());

    let loaded: CookieStorage = persistence
        .read(SessionFile::Cookies)
        .expect("保存した内容を読める");
    assert_eq!(
        serde_json::to_value(&loaded).expect("比較用に変換できる"),
        serde_json::to_value(&jar).expect("比較用に変換できる"),
        "往復で内容が変わらない",
    );
}

#[test]
fn tampered_file_is_rejected_and_stops_saving() {
    let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
    let persistence = SessionPersistence::from_key(dir.path().to_path_buf(), TEST_KEY);
    let jar = cookie_jar_with_secret();
    persistence
        .write(SessionFile::Cookies, &jar)
        .expect("暗号化して保存できる");

    // 末尾 1 byte を壊し、認証タグ検証を失敗させる。
    let path = dir.path().join("cookie_jar.json");
    let mut bytes = fs::read(&path).expect("保存内容を読める");
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    fs::write(&path, &bytes).expect("改ざん内容を書ける");

    let loaded: Option<CookieStorage> = persistence.read(SessionFile::Cookies);
    assert!(loaded.is_none(), "改ざんデータは読まない");
    assert!(
        !persistence.is_enabled(SessionFile::Cookies),
        "失敗したファイルの保存を停止する",
    );
    assert!(
        persistence.write(SessionFile::Cookies, &jar).is_err(),
        "停止したファイルへは書けない",
    );
    assert_eq!(
        fs::read(&path).expect("再読できる"),
        bytes,
        "失敗時に元ファイルを変えない",
    );
}

#[test]
fn plaintext_is_migrated_to_encrypted_form() {
    let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
    let jar = cookie_jar_with_secret();
    let plain_path = dir.path().join("cookie_jar.json");
    serde_json::to_writer_pretty(fs::File::create(&plain_path).expect("旧平文を置ける"), &jar)
        .expect("旧平文を書ける");

    let persistence = SessionPersistence::from_key(dir.path().to_path_buf(), TEST_KEY);
    let loaded: CookieStorage = persistence
        .read(SessionFile::Cookies)
        .expect("旧平文を読める");
    assert_eq!(
        serde_json::to_value(&loaded).expect("比較用に変換できる"),
        serde_json::to_value(&jar).expect("比較用に変換できる"),
        "移行時も値は保つ",
    );
    assert!(
        read_file_bytes(dir.path(), "cookie_jar.json").starts_with(b"XRIFTSEC"),
        "読み込みで暗号化形式へ置き換える",
    );
}

#[test]
fn plaintext_is_readable_without_key_but_not_migrated() {
    let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
    let jar = cookie_jar_with_secret();
    serde_json::to_writer_pretty(
        fs::File::create(dir.path().join("cookie_jar.json")).expect("旧平文を置ける"),
        &jar,
    )
    .expect("旧平文を書ける");
    let before = read_file_bytes(dir.path(), "cookie_jar.json");

    // 鍵が無い起動に相当し、移行はしない。
    let persistence = SessionPersistence::without_key(dir.path().to_path_buf());
    let loaded: CookieStorage = persistence
        .read(SessionFile::Cookies)
        .expect("鍵が無くても平文は読める");
    assert_eq!(
        serde_json::to_value(&loaded).expect("比較用に変換できる"),
        serde_json::to_value(&jar).expect("比較用に変換できる"),
        "鍵無しでも値は保つ",
    );
    assert_eq!(
        read_file_bytes(dir.path(), "cookie_jar.json"),
        before,
        "鍵無しではファイルを書き換えない",
    );
}

#[test]
fn swapped_file_kind_is_rejected() {
    let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
    let persistence = SessionPersistence::from_key(dir.path().to_path_buf(), TEST_KEY);
    persistence
        .write(SessionFile::Cookies, &cookie_jar_with_secret())
        .expect("Cookie 用に保存できる");

    // Cookie 用の envelope を auth 用ファイルへ置き、種別 AAD の不一致を起こす。
    fs::rename(
        dir.path().join("cookie_jar.json"),
        dir.path().join("auth_cache.json"),
    )
    .expect("種別を取り違えた配置にできる");

    let loaded: Option<AuthCache> = persistence.read(SessionFile::AuthCache);
    assert!(loaded.is_none(), "種別違いは読まない");
    assert!(
        !persistence.is_enabled(SessionFile::AuthCache),
        "種別違いで保存を停止する",
    );
}

#[test]
fn unknown_format_version_is_rejected() {
    let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
    let persistence = SessionPersistence::from_key(dir.path().to_path_buf(), TEST_KEY);
    persistence
        .write(SessionFile::Cookies, &cookie_jar_with_secret())
        .expect("暗号化して保存できる");

    // 形式バージョン byte を未知値へ変える。
    let path = dir.path().join("cookie_jar.json");
    let mut bytes = fs::read(&path).expect("保存内容を読める");
    bytes[8] = 99;
    fs::write(&path, &bytes).expect("未知バージョンを書ける");

    let loaded: Option<CookieStorage> = persistence.read(SessionFile::Cookies);
    assert!(loaded.is_none(), "未知バージョンは読まない");
    assert!(
        !persistence.is_enabled(SessionFile::Cookies),
        "未知バージョンで保存を停止する",
    );
}

#[test]
fn second_instance_for_same_profile_is_disabled() {
    // 同一プロファイルの同時起動に相当し、後発は永続化しない。
    // 先発の書き込みを後発が壊さないことを確かめる。
    let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
    let first = SessionPersistence::from_key(dir.path().to_path_buf(), TEST_KEY);
    assert!(
        first.is_enabled(SessionFile::Cookies),
        "先発は永続化できる",
    );
    let second = SessionPersistence::from_key(dir.path().to_path_buf(), TEST_KEY);
    assert!(
        !second.is_enabled(SessionFile::Cookies),
        "後発は永続化しない",
    );
    assert!(
        !second.is_enabled(SessionFile::AuthCache),
        "後発は他ファイルも永続化しない",
    );

    let jar = cookie_jar_with_secret();
    first
        .write(SessionFile::Cookies, &jar)
        .expect("先発は書き込める");
    let before = read_file_bytes(dir.path(), "cookie_jar.json");

    assert!(
        second.write(SessionFile::Cookies, &jar).is_err(),
        "無効な後発は書けない",
    );
    assert_eq!(
        read_file_bytes(dir.path(), "cookie_jar.json"),
        before,
        "後発の失敗で元ファイルを変えない",
    );
    assert_no_temp_files(dir.path());
}

#[test]
fn nonce_changes_on_each_save() {
    // 同じ値を連続保存しても nonce が変わり、同一鍵での nonce 再利用を避ける。
    let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
    let persistence = SessionPersistence::from_key(dir.path().to_path_buf(), TEST_KEY);
    let jar = cookie_jar_with_secret();

    persistence
        .write(SessionFile::Cookies, &jar)
        .expect("1回目を保存できる");
    let first = read_file_bytes(dir.path(), "cookie_jar.json");
    persistence
        .write(SessionFile::Cookies, &jar)
        .expect("2回目を保存できる");
    let second = read_file_bytes(dir.path(), "cookie_jar.json");

    assert!(first.len() > 35 && second.len() > 35, "nonce を含む長さである");
    assert_eq!(&first[..11], &second[..11], "ヘッダーは変わらない");
    assert_ne!(
        &first[11..35],
        &second[11..35],
        "保存ごとに nonce を変える",
    );
    assert_no_temp_files(dir.path());
}

#[test]
fn aad_rejects_swapped_kind_even_if_header_kind_is_changed() {
    // Cookies として保存した envelope の kind byte を AuthCache の値へ書き換えても、
    // AAD 不一致で復号自体が失敗し取り違えを防ぐ。ヘッダー不一致の検証とは別物である。
    // Value で読むのは、Cookie の JSON も Value としては妥当なため、
    // AAD を外すと復号・パースが成功して Some になり、AAD による復号失敗を検証できなくなるためである。
    let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
    let persistence = SessionPersistence::from_key(dir.path().to_path_buf(), TEST_KEY);
    persistence
        .write(SessionFile::Cookies, &cookie_jar_with_secret())
        .expect("Cookie 用に保存できる");

    let mut bytes = read_file_bytes(dir.path(), "cookie_jar.json");
    assert_eq!(bytes[10], 1, "Cookie の種別タグである");
    bytes[10] = 2;
    fs::write(dir.path().join("auth_cache.json"), &bytes).expect("種別を偽装した配置にできる");

    let loaded: Option<serde_json::Value> = persistence.read(SessionFile::AuthCache);
    assert!(loaded.is_none(), "種別の偽装は読まない");
    assert!(
        !persistence.is_enabled(SessionFile::AuthCache),
        "偽装で保存を停止する",
    );
}

#[test]
fn encrypted_file_without_key_is_not_overwritten() {
    // 鍵が無い起動では暗号化済みを読まず、平文へ戻さない。
    let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
    let jar = cookie_jar_with_secret();
    let before = {
        let persistence = SessionPersistence::from_key(dir.path().to_path_buf(), TEST_KEY);
        persistence
            .write(SessionFile::Cookies, &jar)
            .expect("暗号化して保存できる");
        read_file_bytes(dir.path(), "cookie_jar.json")
        // ロックを解放してから鍵無しで開く。
    };

    let persistence = SessionPersistence::without_key(dir.path().to_path_buf());
    let loaded: Option<CookieStorage> = persistence.read(SessionFile::Cookies);
    assert!(loaded.is_none(), "鍵無しでは暗号化済みを読まない");
    assert_eq!(
        read_file_bytes(dir.path(), "cookie_jar.json"),
        before,
        "鍵無しではファイルを書き換えない",
    );
    assert_no_temp_files(dir.path());
}

#[test]
fn concurrent_writes_are_serialized() {
    // 複数スレッドからの同時保存が欠損なく直列化される。
    // 全て成功し、最終ファイルが読めることを確かめる。
    let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
    let persistence = Arc::new(SessionPersistence::from_key(
        dir.path().to_path_buf(),
        TEST_KEY,
    ));
    // 時刻で内容が変わらないよう、同一ペイロードを使い回して同時保存する。
    let payload = serde_json::to_value(&cookie_jar_with_secret()).expect("比較用に変換できる");
    let expected = payload.clone();

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let persistence = Arc::clone(&persistence);
            let payload = payload.clone();
            std::thread::spawn(move || persistence.write(SessionFile::Cookies, &payload))
        })
        .collect();
    for handle in handles {
        handle
            .join()
            .expect("スレッドが終了できる")
            .expect("同時保存が全て成功する");
    }

    let loaded: CookieStorage = persistence
        .read(SessionFile::Cookies)
        .expect("同時保存後の内容を読める");
    assert_eq!(
        serde_json::to_value(&loaded).expect("比較用に変換できる"),
        expected,
        "同時保存後も内容が保たれる",
    );
    assert_no_temp_files(dir.path());
}

#[cfg(unix)]
#[test]
fn write_failure_preserves_file_and_disables() {
    // 保存先が書けない場合は元ファイルを保ち、以降の保存を停止する。
    // 最後にパーミッションを戻して後始末する。
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
    let persistence = SessionPersistence::from_key(dir.path().to_path_buf(), TEST_KEY);
    let jar = cookie_jar_with_secret();
    persistence
        .write(SessionFile::Cookies, &jar)
        .expect("暗号化して保存できる");
    let before = read_file_bytes(dir.path(), "cookie_jar.json");

    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555))
        .expect("読み取り専用にできる");
    let result = persistence.write(SessionFile::Cookies, &jar);
    assert!(result.is_err(), "書けない保存は失敗する");
    assert_eq!(
        read_file_bytes(dir.path(), "cookie_jar.json"),
        before,
        "失敗時に元ファイルを変えない",
    );
    assert!(
        !persistence.is_enabled(SessionFile::Cookies),
        "失敗したファイルの保存を停止する",
    );

    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755))
        .expect("パーミッションを戻せる");
}

#[cfg(unix)]
#[test]
fn migration_failure_preserves_plaintext() {
    // 旧平文の移行に失敗しても値は返し、平文のまま保ち、以降の保存を停止する。
    // ロック確保後に読み取り専用にし、最後にパーミッションを戻す。
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
    let jar = cookie_jar_with_secret();
    serde_json::to_writer_pretty(
        fs::File::create(dir.path().join("cookie_jar.json")).expect("旧平文を置ける"),
        &jar,
    )
    .expect("旧平文を書ける");
    let before = read_file_bytes(dir.path(), "cookie_jar.json");

    let persistence = SessionPersistence::from_key(dir.path().to_path_buf(), TEST_KEY);
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555))
        .expect("読み取り専用にできる");

    let loaded: CookieStorage = persistence
        .read(SessionFile::Cookies)
        .expect("移行失敗でも値は返す");
    assert_eq!(
        serde_json::to_value(&loaded).expect("比較用に変換できる"),
        serde_json::to_value(&jar).expect("比較用に変換できる"),
        "移行失敗でも値は保つ",
    );
    assert_eq!(
        read_file_bytes(dir.path(), "cookie_jar.json"),
        before,
        "移行失敗で平文を変えない",
    );
    assert!(
        !persistence.is_enabled(SessionFile::Cookies),
        "移行失敗で保存を停止する",
    );

    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755))
        .expect("パーミッションを戻せる");
}

/// 実の資格情報ストアを使い、同一プロファイルで鍵を再取得できることを確かめる。
///
/// OS の許可プロンプトが出る場合があるため ignore 付きとし、
/// 最後に鍵を削除して後始末する。
#[test]
#[ignore]
fn real_keyring_roundtrip_with_same_profile() {
    let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
    let first = SessionPersistence::new(Some(dir.path()));
    assert!(
        first.is_enabled(SessionFile::AuthCache),
        "実ストアで永続化が有効になる",
    );
    let value = auth_cache_with_secret();
    first
        .write(SessionFile::AuthCache, &value)
        .expect("実ストアの鍵で保存できる");

    // 起動し直しに相当し、同じプロファイル識別子で鍵を再取得する。
    // プロファイルの排他ロックを解放してから開き直す。
    drop(first);
    let second = SessionPersistence::new(Some(dir.path()));
    assert!(
        second.is_enabled(SessionFile::AuthCache),
        "再取得した鍵で永続化が有効になる",
    );
    let loaded: AuthCache = second
        .read(SessionFile::AuthCache)
        .expect("再取得した鍵で読める");
    assert_eq!(
        serde_json::to_value(&loaded).expect("比較用に変換できる"),
        serde_json::to_value(&value).expect("比較用に変換できる"),
        "鍵の再取得後も内容が変わらない",
    );

    second.delete_key_for_testing();
}

/// 実の資格情報ストアを使い、暗号化済みがあるのに鍵が無い場合は新鍵を作らず元ファイルを保つ。
///
/// 鍵を消して開き直しても鍵を再生成しないことと、暗号化済みを上書きしないことを確かめる。
/// OS の許可プロンプトが出る場合があるため ignore 付きとし、
/// 最後に鍵を削除して後始末する。
#[test]
#[ignore]
fn real_keyring_missing_key_does_not_overwrite_encrypted_file() {
    let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
    let first = SessionPersistence::new(Some(dir.path()));
    assert!(
        first.is_enabled(SessionFile::Cookies),
        "実ストアで永続化が有効になる",
    );
    let jar = cookie_jar_with_secret();
    first
        .write(SessionFile::Cookies, &jar)
        .expect("実ストアの鍵で保存できる");
    let before = read_file_bytes(dir.path(), "cookie_jar.json");

    // 鍵を消した状態での再起動に相当し、新鍵の生成対象外になる。
    // プロファイルの排他ロックを解放してから開き直す。
    first.delete_key_for_testing();
    drop(first);
    let second = SessionPersistence::new(Some(dir.path()));
    assert!(
        !second.is_enabled(SessionFile::Cookies),
        "鍵が無い場合は新鍵を作らず永続化しない",
    );
    assert_eq!(
        read_file_bytes(dir.path(), "cookie_jar.json"),
        before,
        "鍵が無くても元ファイルを変えない",
    );

    second.delete_key_for_testing();
}
