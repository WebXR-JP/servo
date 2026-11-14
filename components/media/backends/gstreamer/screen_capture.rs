/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Linux の画面共有。
//!
//! XDG Desktop Portal の ScreenCast v5 に同意 UI を任せ、`OpenPipeWireRemote`
//! が返す fd と `Start` の node_id を `pipewiresrc` へ渡す。portal との通信は
//! 専用スレッドで行い、script thread を塞がない。

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd as StdOwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use gstreamer;
use gstreamer::prelude::*;
use log::{info, warn};
use servo_base::generic_channel::GenericCallback;
use servo_media_streams::capture::{
    Constrain, DisplayCaptureConstraints, MediaStreamError, MediaTrackConstraintSet,
};
use servo_media_streams::registry::MediaStreamId;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{Array, OwnedFd, OwnedObjectPath, OwnedValue, Structure, Value};

use super::media_stream::GStreamerMediaStream;

const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST_IFACE: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
const SESSION_IFACE: &str = "org.freedesktop.portal.Session";

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(0);

/// portal のセッションと、その生存期間中に保持する資源。
///
/// `Connection` をストリームの生存期間中保持し、`Session.Close` も同じ接続で
/// 呼ぶ。portal はセッションを作成した接続からの Close だけを受け付けるため、
/// 接続を drop すると `Unmatched caller` になる。
/// `fd` は pipeline が NULL になった後に drop して close する。
struct PortalCaptureSession {
    connection: Connection,
    /// `CreateSession` 完了前は `None`。
    session: Option<OwnedObjectPath>,
    fd: Option<StdOwnedFd>,
    /// 応答待ちの request。タイムアウト時に `Request.Close` するために保持する。
    request: Option<OwnedObjectPath>,
}

impl PortalCaptureSession {
    fn close(&mut self) {
        if let Some(request) = self.request.take() {
            match self.close_request(&request) {
                Ok(()) => info!("画面共有 portal の Request.Close を送った: {request}"),
                Err(error) => warn!("画面共有 portal の Request.Close に失敗: {error}"),
            }
        }
        if let Some(session) = self.session.take() {
            match self.close_session(&session) {
                Ok(()) => info!("画面共有 portal セッションを Session.Close した: {session}"),
                Err(error) => {
                    warn!("画面共有 portal セッションの Session.Close に失敗: {error}")
                },
            }
        }
        // pipeline は既に NULL。ここで fd を閉じる。
        self.fd.take();
    }

    fn close_request(&self, request: &OwnedObjectPath) -> Result<(), String> {
        let proxy = Proxy::new(
            &self.connection,
            PORTAL_DEST,
            request.as_str(),
            REQUEST_IFACE,
        )
        .map_err(|error| error.to_string())?;
        proxy
            .call::<_, _, ()>("Close", &())
            .map_err(|error| error.to_string())
    }

    fn close_session(&self, session: &OwnedObjectPath) -> Result<(), String> {
        let proxy = Proxy::new(
            &self.connection,
            PORTAL_DEST,
            session.as_str(),
            SESSION_IFACE,
        )
        .map_err(|error| error.to_string())?;
        proxy
            .call::<_, _, ()>("Close", &())
            .map_err(|error| error.to_string())
    }
}

/// `create_display_stream` の非同期入口。portal のダイアログ待ちを含むため
/// 専用スレッドへ移し、結果だけを `callback` で返す。
pub fn start(
    constraints: DisplayCaptureConstraints,
    callback: GenericCallback<Result<MediaStreamId, MediaStreamError>>,
) {
    let fallback = callback.clone();
    let spawn_result = thread::Builder::new()
        .name("Display capture".to_owned())
        .spawn(move || {
            let result = create_display_stream(constraints);
            if let Err(error) = &result {
                info!("画面共有の開始に失敗した: {error:?}");
            }
            let _ = callback.send(result);
        });
    if let Err(error) = spawn_result {
        warn!("画面共有スレッドを作成できない: {error}");
        let _ = fallback.send(Err(MediaStreamError::NotReadable(error.to_string())));
    }
}

fn create_display_stream(
    constraints: DisplayCaptureConstraints,
) -> Result<MediaStreamId, MediaStreamError> {
    let (major, minor, micro, _) = gstreamer::version();
    info!("GStreamer {major}.{minor}.{micro} で画面共有を開始する");

    // 事前チェック: pipewiresrc が無ければ portal のダイアログを出さずに返す。
    ensure_pipewiresrc()?;

    let connection = Connection::session().map_err(|error| {
        info!("セッションバスへ接続できない: {error}");
        MediaStreamError::NotSupported
    })?;

    let has_portal = {
        let dbus = Proxy::new(
            &connection,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .map_err(|error| {
            info!("D-Bus の Proxy を作れない: {error}");
            MediaStreamError::NotSupported
        })?;
        dbus.call::<_, _, bool>("NameHasOwner", &(PORTAL_DEST,))
            .map_err(|error| {
                info!("NameHasOwner が失敗した: {error}");
                MediaStreamError::NotSupported
            })?
    };
    if !has_portal {
        info!("org.freedesktop.portal.Desktop が無いため画面共有できない");
        return Err(MediaStreamError::NotSupported);
    }

    let Some(sender) = connection.unique_name().map(request_sender) else {
        info!("D-Bus の unique name を取得できない");
        return Err(MediaStreamError::NotSupported);
    };

    let mut portal = PortalCaptureSession {
        connection,
        session: None,
        fd: None,
        request: None,
    };
    let screencast = Proxy::new(
        &portal.connection,
        PORTAL_DEST,
        PORTAL_PATH,
        SCREENCAST_IFACE,
    )
    .map_err(|error| {
        info!("ScreenCast の Proxy を作れない: {error}");
        MediaStreamError::NotSupported
    })?;

    let session = match create_session(&mut portal, &screencast, &sender) {
        Ok(session) => session,
        Err(error) => {
            portal.close();
            return Err(error);
        },
    };
    portal.session = Some(session.clone());
    info!("画面共有 portal セッションを作成した: {session}");

    let setup = (|| -> Result<gstreamer::Element, MediaStreamError> {
        select_sources(&mut portal, &screencast, &sender)?;
        let stream = start_session(&mut portal, &screencast, &sender)?;
        info!(
            "画面共有の node id: {} serial={:?}",
            stream.node_id, stream.serial
        );
        let fd = open_pipewire_remote(&screencast, &session)?;
        info!("OpenPipeWireRemote の fd を取得した: {}", fd.as_raw_fd());
        portal.fd = Some(fd);
        let fd = portal
            .fd
            .as_ref()
            .expect("直前に fd を設定している");
        build_capture_bin(&constraints.video, fd, &stream)
    })();

    let bin = match setup {
        Ok(bin) => bin,
        Err(error) => {
            portal.close();
            return Err(error);
        },
    };

    let cleanup: Box<dyn FnOnce() + Send> = Box::new(move || {
        portal.close();
    });
    let id = GStreamerMediaStream::try_create_video_from_with_cleanup(bin, cleanup).map_err(
        |error| {
            MediaStreamError::NotReadable(format!("画面共有パイプラインを作成できない: {error}"))
        },
    )?;
    info!("画面共有ストリームを作成した: id={}", id.id());
    Ok(id)
}

fn request_sender(unique_name: &zbus::names::OwnedUniqueName) -> String {
    unique_name.as_str().trim_start_matches(':').replace('.', "_")
}

/// `pipewiresrc` が見つからない場合に、shell 側で同じ GStreamer ビルドの
/// `libgstpipewire.so` を `GST_PLUGIN_SYSTEM_PATH_1_0` へ入れるよう促す。
fn ensure_pipewiresrc() -> Result<(), MediaStreamError> {
    match gstreamer::ElementFactory::make("pipewiresrc").build() {
        Ok(element) => {
            let plugin = element
                .factory()
                .and_then(|factory| factory.plugin())
                .and_then(|plugin| plugin.filename());
            info!("pipewiresrc を利用できる: plugin={plugin:?}");
            Ok(())
        },
        Err(error) => {
            warn!(
                "pipewiresrc が無いため画面共有できない。GST_PLUGIN_SYSTEM_PATH_1_0 に \
                 同じ GStreamer ビルドの libgstpipewire.so を含めること: {error}"
            );
            Err(MediaStreamError::NotSupported)
        },
    }
}

fn next_token(prefix: &str) -> String {
    let count = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{}_{}", std::process::id(), count)
}

fn request_path(sender: &str, token: &str) -> String {
    format!("/org/freedesktop/portal/desktop/request/{sender}/{token}")
}

fn string_value(value: &str) -> OwnedValue {
    OwnedValue::try_from(Value::from(value)).expect("文字列を variant に変換できる")
}

fn object_path_value(value: &OwnedValue) -> Option<OwnedObjectPath> {
    if let Ok(path) = OwnedObjectPath::try_from(value.try_clone().ok()?) {
        return Some(path);
    }
    let string = String::try_from(value.try_clone().ok()?).ok()?;
    OwnedObjectPath::try_from(string).ok()
}

fn response_result(
    code: u32,
    results: HashMap<String, OwnedValue>,
    method: &str,
) -> Result<HashMap<String, OwnedValue>, MediaStreamError> {
    match code {
        0 => Ok(results),
        1 => Err(MediaStreamError::NotAllowed),
        _ => Err(MediaStreamError::NotReadable(format!(
            "{method} が code {code} を返した"
        ))),
    }
}

fn start_request_timeout() -> Duration {
    std::env::var("XRIFT_DISPLAY_CAPTURE_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(600))
}

/// `Response` を購読してから method を呼ぶ。
///
/// zbus blocking の `SignalIterator::next()` は中断できないため、購読と応答待ちは
/// 別スレッドに任せ、呼び出し側は `recv_timeout` で待つ。タイムアウト時は
/// `portal.request` を残し、呼び出し元の `close()` が `Request.Close` →
/// `Session.Close` を行う。
fn call_request<B>(
    portal: &mut PortalCaptureSession,
    screencast: &Proxy<'_>,
    sender: &str,
    token: &str,
    method: &str,
    body: &B,
    timeout: Duration,
) -> Result<HashMap<String, OwnedValue>, MediaStreamError>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let predicted = request_path(sender, token);
    portal.request = Some(
        OwnedObjectPath::try_from(predicted.clone())
            .map_err(|error| MediaStreamError::NotReadable(error.to_string()))?,
    );

    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let (result_tx, result_rx) = mpsc::channel::<Result<(u32, HashMap<String, OwnedValue>), String>>();
    let connection = portal.connection.clone();
    let predicted_for_thread = predicted.clone();
    thread::Builder::new()
        .name("Display capture response".to_owned())
        .spawn(move || {
            let proxy = match Proxy::new_owned(
                connection,
                PORTAL_DEST,
                predicted_for_thread,
                REQUEST_IFACE,
            ) {
                Ok(proxy) => proxy,
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                    return;
                },
            };
            let mut signals = match proxy.receive_signal("Response") {
                Ok(signals) => signals,
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                    return;
                },
            };
            let _ = ready_tx.send(Ok(()));
            let result = match signals.next() {
                Some(message) => message
                    .body()
                    .deserialize::<(u32, HashMap<String, OwnedValue>)>()
                    .map_err(|error| error.to_string()),
                None => Err("portal の応答シグナルが途切れた".to_owned()),
            };
            let _ = result_tx.send(result);
        })
        .map_err(|error| MediaStreamError::NotReadable(error.to_string()))?;

    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => {},
        Ok(Err(error)) => return Err(MediaStreamError::NotReadable(error)),
        Err(RecvTimeoutError::Timeout) => {
            return Err(MediaStreamError::NotReadable(
                "portal の Response 購読がタイムアウトした".to_owned(),
            ));
        },
        Err(RecvTimeoutError::Disconnected) => {
            return Err(MediaStreamError::NotReadable(
                "portal の Response 購読スレッドが終了した".to_owned(),
            ));
        },
    }

    let actual: OwnedObjectPath = screencast
        .call(method, body)
        .map_err(|error| MediaStreamError::NotReadable(error.to_string()))?;
    if actual.as_str() != predicted {
        // 予測 path を購読しているため、実際の path が違えば応答は来ない。
        // close() が実 request を閉じられるよう、保持する request を差し替える。
        portal.request = Some(actual.clone());
        return Err(MediaStreamError::NotReadable(format!(
            "{method} の request path が予測と異なる: actual={} predicted={predicted}",
            actual.as_str()
        )));
    }

    match result_rx.recv_timeout(timeout) {
        Ok(Ok((code, results))) => {
            portal.request = None;
            Ok(response_result(code, results, method)?)
        },
        Ok(Err(error)) => {
            portal.request = None;
            Err(MediaStreamError::NotReadable(error))
        },
        Err(RecvTimeoutError::Timeout) => Err(MediaStreamError::NotReadable(format!(
            "{method} がタイムアウトした"
        ))),
        Err(RecvTimeoutError::Disconnected) => Err(MediaStreamError::NotReadable(format!(
            "{method} の応答待ちスレッドが終了した"
        ))),
    }
}

fn create_session(
    portal: &mut PortalCaptureSession,
    screencast: &Proxy<'_>,
    sender: &str,
) -> Result<OwnedObjectPath, MediaStreamError> {
    let handle_token = next_token("create");
    let session_token = next_token("session");
    let mut options: HashMap<String, OwnedValue> = HashMap::new();
    options.insert("handle_token".to_owned(), string_value(&handle_token));
    options.insert(
        "session_handle_token".to_owned(),
        string_value(&session_token),
    );

    let results = call_request(
        portal,
        screencast,
        sender,
        &handle_token,
        "CreateSession",
        &(options,),
        Duration::from_secs(60),
    )?;
    let handle = results.get("session_handle").ok_or_else(|| {
        MediaStreamError::NotReadable("CreateSession の応答に session_handle が無い".to_owned())
    })?;
    object_path_value(handle).ok_or_else(|| {
        MediaStreamError::NotReadable("session_handle を object path として読めない".to_owned())
    })
}

fn select_sources(
    portal: &mut PortalCaptureSession,
    screencast: &Proxy<'_>,
    sender: &str,
) -> Result<(), MediaStreamError> {
    let handle_token = next_token("select");
    let mut options: HashMap<String, OwnedValue> = HashMap::new();
    options.insert("handle_token".to_owned(), string_value(&handle_token));
    // monitor | window。単一選択、カーソルは映像に埋め込む。
    options.insert("types".to_owned(), OwnedValue::from(3u32));
    options.insert("multiple".to_owned(), OwnedValue::from(false));
    options.insert("cursor_mode".to_owned(), OwnedValue::from(2u32));
    let session = portal
        .session
        .clone()
        .ok_or_else(|| MediaStreamError::NotReadable("portal セッションが無い".to_owned()))?;

    call_request(
        portal,
        screencast,
        sender,
        &handle_token,
        "SelectSources",
        &(session, options),
        Duration::from_secs(60),
    )?;
    Ok(())
}

#[derive(Debug)]
struct StreamInfo {
    node_id: u32,
    serial: Option<String>,
    source_type: Option<u32>,
}

fn start_session(
    portal: &mut PortalCaptureSession,
    screencast: &Proxy<'_>,
    sender: &str,
) -> Result<StreamInfo, MediaStreamError> {
    let handle_token = next_token("start");
    let mut options: HashMap<String, OwnedValue> = HashMap::new();
    options.insert("handle_token".to_owned(), string_value(&handle_token));
    let session = portal
        .session
        .clone()
        .ok_or_else(|| MediaStreamError::NotReadable("portal セッションが無い".to_owned()))?;

    let results = call_request(
        portal,
        screencast,
        sender,
        &handle_token,
        "Start",
        &(session, "", options),
        start_request_timeout(),
    )?;
    let (stream, streams) = first_stream(&results).ok_or_else(|| {
        MediaStreamError::NotReadable("Start の応答に画面共有ストリームが無い".to_owned())
    })?;
    info!(
        "Start の streams: size={streams} source_type={:?}",
        stream.source_type
    );
    Ok(stream)
}

/// `streams` は `a(ua{sv})`。複数来ても先頭だけを使う。
fn first_stream(results: &HashMap<String, OwnedValue>) -> Option<(StreamInfo, usize)> {
    let streams = results.get("streams")?;
    let array = Array::try_from(streams.try_clone().ok()?).ok()?;
    let size = array.len();
    let value = array.inner().first()?.try_clone().ok()?;
    let structure = Structure::try_from(value).ok()?;
    let fields = structure.fields();
    let node_id = match fields.first()? {
        Value::U32(node_id) => *node_id,
        _ => return None,
    };

    let mut serial = None;
    let mut source_type = None;
    if let Some(Value::Dict(dict)) = fields.get(1) {
        let serial_key = String::from("pipewire-serial");
        if let Ok(Some(value)) = dict.get::<String, OwnedValue>(&serial_key) {
            serial = serial_string(&value);
        }
        let source_type_key = String::from("source_type");
        if let Ok(Some(value)) = dict.get::<String, &u32>(&source_type_key) {
            source_type = Some(*value);
        }
    }
    Some((
        StreamInfo {
            node_id,
            serial,
            source_type,
        },
        size,
    ))
}

fn serial_string(value: &OwnedValue) -> Option<String> {
    if let Ok(serial) = u64::try_from(value.try_clone().ok()?) {
        return Some(serial.to_string());
    }
    if let Ok(serial) = u32::try_from(value.try_clone().ok()?) {
        return Some(serial.to_string());
    }
    String::try_from(value.try_clone().ok()?).ok()
}

fn open_pipewire_remote(
    screencast: &Proxy<'_>,
    session: &OwnedObjectPath,
) -> Result<StdOwnedFd, MediaStreamError> {
    let options: HashMap<String, OwnedValue> = HashMap::new();
    let fd: OwnedFd = screencast
        .call("OpenPipeWireRemote", &(session.clone(), options))
        .map_err(|error| {
            info!("OpenPipeWireRemote が失敗した: {error}");
            MediaStreamError::NotSupported
        })?;
    Ok(fd.into())
}

fn target_u32(constraint: Option<&Constrain<u32>>) -> Option<u32> {
    match constraint? {
        Constrain::Value(value) | Constrain::Ideal(value) => Some(*value),
        // display では max だけの範囲制約を解像度の固定値に使わない。
        Constrain::Range(_) => None,
    }
}

fn target_frame_rate(constraint: Option<&Constrain<f64>>) -> Option<gstreamer::Fraction> {
    match constraint? {
        Constrain::Value(value) | Constrain::Ideal(value) => {
            gstreamer::Fraction::approximate_f64(*value)
        },
        Constrain::Range(_) => None,
    }
}

fn make_element(name: &str) -> Result<gstreamer::Element, MediaStreamError> {
    gstreamer::ElementFactory::make(name)
        .build()
        .map_err(|error| MediaStreamError::NotReadable(error.to_string()))
}

/// pipewiresrc から I420 までの変換 bin を作り、ghost src pad を付ける。
/// 呼び出し側の `create_shared_with_device` が bin を tee へ link する。
fn build_capture_bin(
    constraints: &MediaTrackConstraintSet,
    fd: &StdOwnedFd,
    stream: &StreamInfo,
) -> Result<gstreamer::Element, MediaStreamError> {
    let source = gstreamer::ElementFactory::make("pipewiresrc")
        // pipewiresrc は接続時に fd を dup するため、こちらは保持したままにする。
        .property("fd", fd.as_raw_fd())
        .property("client-name", "xrift-client")
        .build()
        .map_err(|error| MediaStreamError::NotReadable(error.to_string()))?;

    let node = stream.node_id.to_string();
    if source.find_property("path").is_some() {
        // portal v5 は node id だけを返す。gstpipewiresrc の path は
        // pw_stream_connect の target_id（node id）に対応する。
        source.set_property("path", node);
        info!("pipewiresrc に path={} を設定した", stream.node_id);
    } else if let Some(serial) = stream.serial.as_ref() {
        // 将来 path が無い GStreamer 向けのフォールバック。v5 では使わない。
        if source.find_property("target-object").is_some() {
            source.set_property("target-object", serial.clone());
            info!("pipewiresrc に target-object={serial} を設定した");
        } else {
            return Err(MediaStreamError::NotReadable(
                "pipewiresrc に path も target-object も無い".to_owned(),
            ));
        }
    } else {
        return Err(MediaStreamError::NotReadable(
            "pipewiresrc に path が無く、pipewire-serial も無い".to_owned(),
        ));
    }

    let bin = gstreamer::Bin::new();
    let videoconvert = make_element("videoconvert")?;
    let videoscale = make_element("videoscale")?;
    let videorate = make_element("videorate")?;

    let mut caps = gstreamer::Caps::builder("video/x-raw");
    if let Some(width) = target_u32(constraints.width.as_ref()) {
        caps = caps.field("width", width as i32);
    }
    if let Some(height) = target_u32(constraints.height.as_ref()) {
        caps = caps.field("height", height as i32);
    }
    if let Some(frame_rate) = target_frame_rate(constraints.frame_rate.as_ref()) {
        caps = caps.field("framerate", frame_rate);
    }
    let capsfilter = gstreamer::ElementFactory::make("capsfilter")
        .property("caps", caps.build())
        .build()
        .map_err(|error| MediaStreamError::NotReadable(error.to_string()))?;

    let videoconvert_i420 = make_element("videoconvert")?;
    let format_filter = gstreamer::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gstreamer::Caps::builder("video/x-raw")
                .field("format", "I420")
                .build(),
        )
        .build()
        .map_err(|error| MediaStreamError::NotReadable(error.to_string()))?;

    let elements: [&gstreamer::Element; 7] = [
        &source,
        &videoconvert,
        &videoscale,
        &videorate,
        &capsfilter,
        &videoconvert_i420,
        &format_filter,
    ];
    bin.add_many(elements)
        .map_err(|error| MediaStreamError::NotReadable(error.to_string()))?;
    gstreamer::Element::link_many(elements)
        .map_err(|error| MediaStreamError::NotReadable(error.to_string()))?;

    let src_pad = format_filter.static_pad("src").ok_or_else(|| {
        MediaStreamError::NotReadable("画面共有 bin の出力 pad が無い".to_owned())
    })?;
    let ghost = gstreamer::GhostPad::with_target(&src_pad)
        .map_err(|error| MediaStreamError::NotReadable(error.to_string()))?;
    bin.add_pad(&ghost)
        .map_err(|error| MediaStreamError::NotReadable(error.to_string()))?;

    Ok(bin.upcast())
}
