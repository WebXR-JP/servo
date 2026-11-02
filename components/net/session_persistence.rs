/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Cookie と HTTP auth の保存データを暗号化して永続化する。
//!
//! 暗号化対象は `cookie_jar.json` と `auth_cache.json` のみである。
//! 鍵は OS の資格情報ストアに置き、起動時に一度だけ取得してプロセス内で共有する。
//! 資格情報ストアが使えない場合や鍵が無い場合は平文で書かず、そのファイルの
//! 永続化を停止する。起動自体は継続する。

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use fs4::FileExt;
use log::{trace, warn};
use parking_lot::Mutex;
use serde::{Serialize, de::DeserializeOwned};

/// XChaCha20-Poly1305 の鍵長。
pub const KEY_LEN: usize = 32;

/// 暗号化対象のセッションファイル。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionFile {
    Cookies,
    AuthCache,
}

impl SessionFile {
    /// プロファイル直下のファイル名を返す。
    fn filename(self) -> &'static str {
        match self {
            SessionFile::Cookies => "cookie_jar.json",
            SessionFile::AuthCache => "auth_cache.json",
        }
    }

    /// エンベロープに埋めるデータ種別タグを返す。
    /// 取り違え検出のため Cookie と auth で値を変える。
    fn kind_tag(self) -> u8 {
        match self {
            SessionFile::Cookies => 1,
            SessionFile::AuthCache => 2,
        }
    }
}

/// ファイル先頭の形式識別子。平文 JSON（`{` 始まり）と区別できる値にする。
const MAGIC: &[u8; 8] = b"XRIFTSEC";
/// エンベロープの形式バージョン。読み書き両対応はこの値のみ。
const FORMAT_VERSION: u8 = 1;
/// 鍵のバージョン。将来の鍵更新に備えた予約であり、現在は 1 のみ扱う。
const KEY_VERSION: u8 = 1;
/// ヘッダー長。形式識別 8 + 形式バージョン 1 + 鍵バージョン 1 + 種別 1。
const HEADER_LEN: usize = 11;
const NONCE_LEN: usize = 24;
/// プロファイル識別子のファイル名。秘密情報ではないため平文で置く。
const PROFILE_ID_FILE: &str = "session-profile-id";
/// 同一プロファイルの同時起動を抑止する排他ロックのファイル名。
/// プロセス終了まで保持し、後発プロセスは永続化を無効化して起動を継続する。
const PROFILE_LOCK_FILE: &str = "session-persistence.lock";
/// プロファイル識別子のバイト長。鍵とは別物であり、鍵導出には使わない。
const PROFILE_ID_LEN: usize = 16;
const KEYRING_SERVICE: &str = "xrift-client";

/// 起動時・読み込み時の警告を出す。ロガー初期化前でも見えるよう、
/// ロガーが無効な場合は stderr に出す。
fn warn_session(message: impl std::fmt::Display) {
    if log::max_level() == log::LevelFilter::Off {
        eprintln!("warning: {message}");
    } else {
        warn!("{message}");
    }
}

/// 資格情報ストア上のアカウント名を作る。
fn account_name(profile_id: &str) -> String {
    format!("session-key:v1:{profile_id}")
}

struct Inner {
    key: [u8; KEY_LEN],
    /// 読み書きに失敗したファイル。以降の保存を止め、空データでの上書きを防ぐ。
    disabled: Mutex<HashSet<SessionFile>>,
    /// 保存の「無効確認〜rename 完了」を永続化全体で直列化する。
    /// 取得順は `write_lock` → `disabled` に固定し、逆順を作らない。
    write_lock: Mutex<()>,
    /// 一時ファイル名を保存ごとに一意にするための連番。
    temp_seq: AtomicU64,
}

/// セッション情報の暗号化永続化。鍵は起動時に一度だけ取得し共有する。
///
/// 同一プロファイルの同時起動に備え、プロファイル直下のロックファイルを
/// プロセス終了まで排他保持する。後発プロセスは `inner` を持たず永続化しない。
pub struct SessionPersistence {
    config_dir: Option<PathBuf>,
    inner: Option<Inner>,
    /// プロファイルの排他ロック。保持し続けることで同時起動を抑止する。
    /// 無効状態や鍵無しでは保持しない。
    _lock: Option<File>,
}

impl SessionPersistence {
    /// プロファイルディレクトリから鍵を取得して初期化する。
    ///
    /// 資格情報ストアが使えない場合や鍵が無い場合は `inner` を持たず、
    /// そのファイルの永続化を停止する。起動自体は継続する。
    /// 同一プロファイルの二重起動に備え、鍵取得より先に排他ロックを取る。
    /// ロック取得や鍵取得に失敗した場合はロックを保持せず解放する。
    pub fn new(config_dir: Option<&Path>) -> Self {
        let Some(dir) = config_dir.map(Path::to_path_buf) else {
            return Self::disabled();
        };
        // 鍵の二重生成や暗号化データの判定競合を避けるため、鍵取得より先に抑止する。
        let Some(lock) = acquire_profile_lock(&dir) else {
            return Self {
                config_dir: Some(dir),
                inner: None,
                _lock: None,
            };
        };
        match load_key(&dir) {
            Some(key) => Self {
                config_dir: Some(dir),
                inner: Some(Inner {
                    key,
                    disabled: Mutex::new(HashSet::new()),
                    write_lock: Mutex::new(()),
                    temp_seq: AtomicU64::new(0),
                }),
                _lock: Some(lock),
            },
            // 無効状態では書き込まないため、他プロセスの邪魔にならないよう解放する。
            None => Self {
                config_dir: Some(dir),
                inner: None,
                _lock: None,
            },
        }
    }

    /// 永続化しないインスタンスを作る。private 用である。
    pub fn disabled() -> Self {
        Self {
            config_dir: None,
            inner: None,
            _lock: None,
        }
    }

    /// ファイルを読み込む。存在しない場合は `None` を返す。
    ///
    /// 旧平文は読み込み時に暗号化形式へ原子置換する。移行に失敗しても
    /// 読み込んだ値は返し、以降の保存は停止する（元ファイルは維持する）。
    /// `inner` が無い場合も平文は読めるが、移行はしない。
    pub fn read<T>(&self, file: SessionFile) -> Option<T>
    where
        T: DeserializeOwned + Serialize,
    {
        let dir = self.config_dir.as_deref()?;
        let bytes = match fs::read(dir.join(file.filename())) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return None,
            Err(error) => {
                warn_session(format!(
                    "{} を読めないため {} の永続化を停止する: {error}",
                    dir.join(file.filename()).display(),
                    file.filename(),
                ));
                self.disable(file);
                return None;
            },
        };
        if bytes.starts_with(MAGIC) {
            return self.read_envelope(file, &bytes);
        }
        match serde_json::from_slice::<T>(&bytes) {
            Ok(value) => {
                // 旧平文の移行。鍵がある場合のみ暗号化形式へ置き換える。
                if self.is_enabled(file) {
                    if let Err(error) = self.write(file, &value) {
                        warn_session(format!("{} を暗号化形式へ移行できない: {error}", file.filename()));
                    }
                }
                Some(value)
            },
            Err(error) => {
                warn_session(format!(
                    "{} を平文として読めないため {} の永続化を停止する: {error}",
                    file.filename(),
                    file.filename(),
                ));
                self.disable(file);
                None
            },
        }
    }

    /// 値を暗号化して原子置換で保存する。
    ///
    /// # Errors
    ///
    /// 永続化が無効な場合、シリアライズ・暗号化・書き込みに失敗した場合に
    /// エラーを返す。失敗したファイルは以降の保存も停止する。
    pub fn write<T: Serialize>(&self, file: SessionFile, value: &T) -> std::io::Result<()> {
        let (Some(dir), Some(inner)) = (self.config_dir.as_deref(), self.inner.as_ref()) else {
            return Err(std::io::Error::other("session persistence is disabled"));
        };
        // ファイル単位ではなく永続化全体で直列化し、一時ファイルの衝突を防ぐ。
        // 取得順は `write_lock` → `disabled` に固定する。
        let _write_guard = inner.write_lock.lock();
        if inner.disabled.lock().contains(&file) {
            return Err(std::io::Error::other("session persistence is disabled"));
        }
        let plaintext = serde_json::to_vec_pretty(value).map_err(|error| {
            self.disable(file);
            std::io::Error::other(error)
        })?;
        // 保存ごとに新しい nonce を使う。使い回すと同一鍵での安全性が崩れる。
        let mut nonce = [0u8; NONCE_LEN];
        rand::fill(&mut nonce);
        let mut header = [0u8; HEADER_LEN];
        header[..MAGIC.len()].copy_from_slice(MAGIC);
        header[8] = FORMAT_VERSION;
        header[9] = KEY_VERSION;
        header[10] = file.kind_tag();
        let cipher = XChaCha20Poly1305::new_from_slice(&inner.key).map_err(|error| {
            self.disable(file);
            std::io::Error::other(error.to_string())
        })?;
        // 形式識別〜データ種別を AAD に含め、取り違え・改ざんを検出する。
        let ciphertext = cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &plaintext,
                    aad: &header,
                },
            )
            .map_err(|error| {
                self.disable(file);
                std::io::Error::other(error.to_string())
            })?;
        let mut envelope = Vec::with_capacity(header.len() + nonce.len() + ciphertext.len());
        envelope.extend_from_slice(&header);
        envelope.extend_from_slice(&nonce);
        envelope.extend_from_slice(&ciphertext);
        // 書き込み中の強制終了で壊れた JSON を残さないよう一時ファイル + rename で置き換える。
        // プロセス内スレッド間でも一時ファイル名が重ならないよう保存ごとに一意にする。
        let filename = file.filename();
        let path = dir.join(filename);
        let seq = inner.temp_seq.fetch_add(1, Ordering::Relaxed);
        let temp_path = dir.join(format!("{filename}.{}.{seq}.tmp", std::process::id()));
        let result = (|| {
            let mut temp = fs::File::create(&temp_path)?;
            temp.write_all(&envelope)?;
            temp.flush()?;
            fs::rename(&temp_path, &path)?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                trace!("{} を暗号化して保存した", file.filename());
                Ok(())
            },
            Err(error) => {
                let _ = fs::remove_file(&temp_path);
                self.disable(file);
                Err(error)
            },
        }
    }

    /// そのファイルの永続化が有効かを返す。
    pub fn is_enabled(&self, file: SessionFile) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| !inner.disabled.lock().contains(&file))
    }

    fn read_envelope<T>(&self, file: SessionFile, bytes: &[u8]) -> Option<T>
    where
        T: DeserializeOwned,
    {
        let Some(inner) = self.inner.as_ref() else {
            warn_session(format!(
                "{} は暗号化されているが鍵が無いため読まない",
                file.filename(),
            ));
            return None;
        };
        if bytes.len() < HEADER_LEN + NONCE_LEN {
            warn_session(format!(
                "{} の形式が不正なため {} の永続化を停止する",
                file.filename(),
                file.filename(),
            ));
            self.disable(file);
            return None;
        }
        if bytes[8] != FORMAT_VERSION || bytes[9] != KEY_VERSION || bytes[10] != file.kind_tag() {
            warn_session(format!(
                "{} の形式・鍵・種別が想定外のため {} の永続化を停止する",
                file.filename(),
                file.filename(),
            ));
            self.disable(file);
            return None;
        }
        let header = &bytes[..HEADER_LEN];
        let nonce: [u8; NONCE_LEN] = bytes[HEADER_LEN..HEADER_LEN + NONCE_LEN]
            .try_into()
            .expect("長さ検査済みのため変換できる");
        let ciphertext = &bytes[HEADER_LEN + NONCE_LEN..];
        let cipher = XChaCha20Poly1305::new_from_slice(&inner.key).ok()?;
        let plaintext = cipher
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: ciphertext,
                    aad: header,
                },
            )
            .inspect_err(|_| {
                warn_session(format!(
                    "{} の復号に失敗したため {} の永続化を停止する",
                    file.filename(),
                    file.filename(),
                ));
                self.disable(file);
            })
            .ok()?;
        serde_json::from_slice::<T>(&plaintext)
            .inspect_err(|error| {
                warn_session(format!(
                    "{} の復号結果を読めないため {} の永続化を停止する: {error}",
                    file.filename(),
                    file.filename(),
                ));
                self.disable(file);
            })
            .ok()
    }

    fn disable(&self, file: SessionFile) {
        if let Some(inner) = self.inner.as_ref() {
            inner.disabled.lock().insert(file);
        }
    }

    /// テスト用に鍵を直接指定して作る。資格情報ストアを使わない。
    ///
    /// 本番の `new` と同じくプロファイルの排他ロックを通す。
    /// 同一ディレクトリで二つ目を作ると無効状態になる。
    #[cfg(any(test, feature = "test-util"))]
    pub fn from_key(config_dir: PathBuf, key: [u8; KEY_LEN]) -> Self {
        let Some(lock) = acquire_profile_lock(&config_dir) else {
            return Self {
                config_dir: Some(config_dir),
                inner: None,
                _lock: None,
            };
        };
        Self {
            config_dir: Some(config_dir),
            inner: Some(Inner {
                key,
                disabled: Mutex::new(HashSet::new()),
                write_lock: Mutex::new(()),
                temp_seq: AtomicU64::new(0),
            }),
            _lock: Some(lock),
        }
    }

    /// テスト用に鍵無しで作る。資格情報ストアを使わない。
    ///
    /// 鍵取得失敗時と同じくロックは保持せず、競合の有無だけ確認する。
    /// 同一ディレクトリで先発がロック保持中の場合は警告の上で無効状態になる。
    #[cfg(any(test, feature = "test-util"))]
    pub fn without_key(config_dir: PathBuf) -> Self {
        // 競合検出のためだけに取得し、鍵が無いため保持せず解放する。
        // 無効状態では書き込まないため、他インスタンスの邪魔にならない。
        // 取得失敗時の警告は `acquire_profile_lock` 側で行う。
        let _ = acquire_profile_lock(&config_dir);
        Self {
            config_dir: Some(config_dir),
            inner: None,
            _lock: None,
        }
    }

    /// テスト用にこのプロファイルの鍵を資格情報ストアから削除する。
    #[cfg(any(test, feature = "test-util"))]
    pub fn delete_key_for_testing(&self) {
        let Some(dir) = self.config_dir.as_deref() else {
            return;
        };
        let Ok(profile_id) = fs::read_to_string(dir.join(PROFILE_ID_FILE)) else {
            return;
        };
        let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &account_name(profile_id.trim()))
        else {
            return;
        };
        if let Err(error) = entry.delete_credential() {
            warn_session(format!("テスト用鍵の削除に失敗した: {error}"));
        }
    }
}

/// プロファイル単位の排他ロックを取得する。
///
/// 同一プロファイルの同時起動で鍵の二重生成や暗号化データの判定競合が起きないよう、
/// プロファイル直下のロックファイルを排他ロックする。保持し続けることで抑止し、
/// 取得失敗時は警告して `None` を返す。呼び出し側は永続化を無効化して起動を継続する。
fn acquire_profile_lock(config_dir: &Path) -> Option<File> {
    let path = config_dir.join(PROFILE_LOCK_FILE);
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) => {
            warn_session(format!(
                "{} を開けないためセッションの永続化を停止する: {error}",
                path.display(),
            ));
            return None;
        },
    };
    // `std::fs::File` にも同名の inherent メソッドがあるため、fs4 の排他ロックを明示する。
    if let Err(error) = <File as FileExt>::try_lock(&file) {
        warn_session(format!(
            "{} を排他ロックできないためセッションの永続化を停止する: {error}",
            path.display(),
        ));
        return None;
    }
    Some(file)
}

/// プロファイルに対応する鍵を取得する。取得できない場合は `None` を返す。
///
/// 暗号化済みデータがあるのに鍵が無い場合は新規生成せず `None` を返す。
/// 対応する鍵なしに作った鍵で上書きすると既存データを二度と読めなくなるためである。
fn load_key(config_dir: &Path) -> Option<[u8; KEY_LEN]> {
    if let Err(error) = keyring::Entry::store_status() {
        warn_session(format!("資格情報ストアが使えないためセッションの永続化を停止する: {error}"));
        return None;
    }
    // 読み取れないファイルは暗号化済み・破損の可能性があるため、
    // 新鍵の生成対象外として扱い既存データを壊さない。
    let has_encrypted = [SessionFile::Cookies, SessionFile::AuthCache]
        .iter()
        .any(|file| file_looks_encrypted(config_dir, *file));
    let profile_id = load_or_create_profile_id(config_dir, has_encrypted)?;
    let entry = match keyring::Entry::new(KEYRING_SERVICE, &account_name(&profile_id)) {
        Ok(entry) => entry,
        Err(error) => {
            warn_session(format!("資格情報ストアの項目を作れないためセッションの永続化を停止する: {error}"));
            return None;
        },
    };
    match entry.get_secret() {
        Ok(secret) => {
            if secret.len() != KEY_LEN {
                warn_session("資格情報ストアの鍵長が想定外のためセッションの永続化を停止する");
                return None;
            }
            let mut key = [0u8; KEY_LEN];
            key.copy_from_slice(&secret);
            Some(key)
        },
        Err(keyring::Error::NoEntry) => {
            if has_encrypted {
                warn_session("暗号化済みデータに対応する鍵が無いため上書きせず永続化を停止する");
                return None;
            }
            let mut key = [0u8; KEY_LEN];
            rand::fill(&mut key);
            if let Err(error) = entry.set_secret(&key) {
                warn_session(format!("鍵を保存できないためセッションの永続化を停止する: {error}"));
                return None;
            }
            // 保存直後に読み返し、同じ値であることを確認してから使う。
            match entry.get_secret() {
                Ok(stored) if stored == key => Some(key),
                Ok(_) => {
                    warn_session("保存した鍵と読み返した鍵が異なるためセッションの永続化を停止する");
                    None
                },
                Err(error) => {
                    warn_session(format!("保存した鍵を読めないためセッションの永続化を停止する: {error}"));
                    None
                },
            }
        },
        Err(error) => {
            warn_session(format!("鍵を取得できないためセッションの永続化を停止する: {error}"));
            None
        },
    }
}

/// ファイルが暗号化形式に見えるかを返す。
/// 存在しない場合は偽、読めない場合は暗号化済み扱い（真）とする。
fn file_looks_encrypted(config_dir: &Path, file: SessionFile) -> bool {
    match fs::read(config_dir.join(file.filename())) {
        Ok(bytes) => bytes.starts_with(MAGIC),
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// プロファイル識別子を取得する。無ければ生成して保存する。
///
/// 暗号化済みデータがあるのに識別子が無い場合は新規作成せず `None` を返す。
/// 識別子が変わると鍵のアカウント名が変わり既存データを二度と読めなくなるためである。
fn load_or_create_profile_id(config_dir: &Path, has_encrypted: bool) -> Option<String> {
    let path = config_dir.join(PROFILE_ID_FILE);
    match fs::read_to_string(&path) {
        Ok(content) => {
            let id = content.trim().to_owned();
            let valid = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(&id)
                .is_ok_and(|bytes| bytes.len() == PROFILE_ID_LEN);
            if !valid {
                warn_session("プロファイル識別子が不正なためセッションの永続化を停止する");
                return None;
            }
            Some(id)
        },
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if has_encrypted {
                warn_session("暗号化済みデータがあるがプロファイル識別子が無いため永続化を停止する");
                return None;
            }
            let mut bytes = [0u8; PROFILE_ID_LEN];
            rand::fill(&mut bytes);
            let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
            // ロック下のため競合はないが、クラッシュ時の破損を避けるため原子置換する。
            let temp_path =
                config_dir.join(format!("{PROFILE_ID_FILE}.{}.tmp", std::process::id()));
            let written = (|| {
                let mut temp = File::create(&temp_path)?;
                temp.write_all(id.as_bytes())?;
                temp.flush()?;
                fs::rename(&temp_path, &path)?;
                Ok::<(), std::io::Error>(())
            })();
            if let Err(error) = written {
                let _ = fs::remove_file(&temp_path);
                warn_session(format!("プロファイル識別子を保存できないためセッションの永続化を停止する: {error}"));
                return None;
            }
            Some(id)
        },
        Err(error) => {
            warn_session(format!("プロファイル識別子を読めないためセッションの永続化を停止する: {error}"));
            None
        },
    }
}
