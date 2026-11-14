/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use gstreamer::prelude::*;
use gstreamer_app::{AppSink, AppSrc};
use servo_media_streams::registry::{
    MediaStreamId, clone_stream, get_stream, set_stream_enabled, stop_stream, stream_settings,
};

use super::{GStreamerMediaStream, SharedSource};

struct TestTrack(MediaStreamId);

struct TestAudioDestinationInput {
    pipeline: gstreamer::Pipeline,
    stop: Arc<AtomicBool>,
    producer: Option<JoinHandle<()>>,
}

impl TestAudioDestinationInput {
    fn new(socket: &super::GstreamerMediaSocket) -> Self {
        let pipeline = gstreamer::Pipeline::with_name("synthetic WebAudio destination input");
        let appsrc = gstreamer::ElementFactory::make("appsrc")
            .property("is-live", true)
            .build()
            .expect("WebAudio destination appsrc を作れない")
            .downcast::<AppSrc>()
            .expect("appsrc の型が不正");
        appsrc.set_caps(Some(
            &gstreamer::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("layout", "interleaved")
                .field("rate", 48_000i32)
                .field("channels", 2i32)
                .build(),
        ));
        appsrc.set_format(gstreamer::Format::Time);
        let convert = gstreamer::ElementFactory::make("audioconvert")
            .build()
            .expect("WebAudio destination audioconvert を作れない");
        let sink = socket.proxy_sink().clone();
        pipeline
            .add_many([appsrc.upcast_ref(), &convert, &sink])
            .expect("WebAudio destination elements を追加できない");
        gstreamer::Element::link_many([appsrc.upcast_ref(), &convert, &sink])
            .expect("WebAudio destination elements を接続できない");
        pipeline
            .set_state(gstreamer::State::Playing)
            .expect("WebAudio destination pipeline を開始できない");

        let stop = Arc::new(AtomicBool::new(false));
        let producer_stop = stop.clone();
        let producer = thread::spawn(move || {
            const FRAMES: usize = 480;
            const CHANNELS: usize = 2;
            let mut frame = 0u64;
            while !producer_stop.load(Ordering::Relaxed) {
                let mut buffer = gstreamer::Buffer::with_size(FRAMES * CHANNELS * 4)
                    .expect("WebAudio destination audio buffer を作れない");
                {
                    let buffer = buffer
                        .get_mut()
                        .expect("新しい audio buffer を変更できない");
                    let start = frame * FRAMES as u64;
                    let end = start + FRAMES as u64;
                    buffer.set_pts(Some(gstreamer::ClockTime::from_nseconds(
                        start * gstreamer::ClockTime::SECOND.nseconds() / 48_000,
                    )));
                    buffer.set_duration(Some(gstreamer::ClockTime::from_nseconds(
                        (end - start) * gstreamer::ClockTime::SECOND.nseconds() / 48_000,
                    )));
                    let mut samples = buffer
                        .map_writable()
                        .expect("WebAudio destination audio buffer を書き込めない");
                    for (sample_index, sample_bytes) in
                        samples.as_mut_slice().chunks_exact_mut(4).enumerate()
                    {
                        let time = (start + (sample_index / CHANNELS) as u64) as f32 / 48_000.0;
                        let sample = (time * 440.0 * std::f32::consts::TAU).sin() * 0.25;
                        sample_bytes.copy_from_slice(&sample.to_le_bytes());
                    }
                }
                if appsrc.push_buffer(buffer).is_err() {
                    break;
                }
                frame += FRAMES as u64;
                thread::sleep(Duration::from_millis(10));
            }
        });

        Self {
            pipeline,
            stop,
            producer: Some(producer),
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(producer) = self.producer.take() {
            producer
                .join()
                .expect("WebAudio destination producer が panic した");
        }
        self.pipeline
            .set_state(gstreamer::State::Null)
            .expect("WebAudio destination pipeline を停止できない");
    }
}

impl Drop for TestAudioDestinationInput {
    fn drop(&mut self) {
        self.stop();
    }
}

impl TestTrack {
    fn audio() -> Self {
        gstreamer::init().expect("GStreamer を初期化できない");
        Self(GStreamerMediaStream::create_audio())
    }

    fn audio_destination() -> (Self, TestAudioDestinationInput) {
        gstreamer::init().expect("GStreamer を初期化できない");
        let (stream, socket) =
            GStreamerMediaStream::try_create_proxy(servo_media_streams::MediaStreamType::Audio)
                .expect("AudioContext の合成音声 stream を作れない");
        let input = TestAudioDestinationInput::new(&socket);
        (Self(stream), input)
    }

    fn clone(&self) -> Self {
        Self(clone_stream(&self.0).expect("音声トラックを複製できない"))
    }

    fn source(&self) -> Arc<Mutex<SharedSource>> {
        let stream = get_stream(&self.0).expect("音声トラックが登録されていない");
        let stream = stream.lock().unwrap();
        stream
            .as_any()
            .downcast_ref::<GStreamerMediaStream>()
            .and_then(|stream| stream.branch.as_ref())
            .expect("音声トラックに入力枝がない")
            .source
            .clone()
    }
}

impl Drop for TestTrack {
    fn drop(&mut self) {
        let _ = stop_stream(&self.0);
    }
}

struct TestConsumer {
    pipeline: gstreamer::Pipeline,
    sink: AppSink,
}

impl TestConsumer {
    fn attach(track: &TestTrack) -> Self {
        let stream = get_stream(&track.0).expect("音声トラックが登録されていない");
        let mut stream = stream.lock().unwrap();
        let stream = stream
            .as_mut_any()
            .downcast_mut::<GStreamerMediaStream>()
            .expect("音声トラックが GStreamer のストリームでない");
        let pipeline = stream.pipeline_or_new();
        let output = stream.src_element();
        let sink = gstreamer::ElementFactory::make("appsink")
            .property("async", false)
            .property("sync", false)
            .property("max-buffers", 4u32)
            .property("drop", true)
            .build()
            .expect("appsink を作れない")
            .downcast::<AppSink>()
            .expect("appsink の型が不正");
        pipeline
            .add(&sink)
            .expect("appsink をパイプラインへ追加できない");
        output
            .link(&sink)
            .expect("音声出力を appsink へ接続できない");
        pipeline
            .set_state(gstreamer::State::Playing)
            .expect("音声出力パイプラインを開始できない");
        Self { pipeline, sink }
    }

    fn pull_bytes(&self) -> Vec<u8> {
        self.pull_bytes_with_timeout(gstreamer::ClockTime::from_seconds(3))
    }

    fn pull_bytes_with_timeout(&self, timeout: gstreamer::ClockTime) -> Vec<u8> {
        let sample = self
            .sink
            .try_pull_sample(timeout)
            .expect("指定時間内に音声サンプルを受信できない");
        let buffer = sample.buffer().expect("appsink のサンプルに buffer がない");
        buffer
            .map_readable()
            .expect("音声 buffer を読み取れない")
            .as_slice()
            .to_vec()
    }

    fn pull_until_silent(&self) -> bool {
        (0..10).any(|_| {
            self.pull_bytes_with_timeout(gstreamer::ClockTime::from_mseconds(500))
                .iter()
                .all(|byte| *byte == 0)
        })
    }
}

impl Drop for TestConsumer {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}

#[test]
fn audio_from_device_reports_device_id_in_settings() {
    gstreamer::init().expect("GStreamer を初期化できない");
    let source = gstreamer::ElementFactory::make("audiotestsrc")
        .property_from_str("wave", "sine")
        .property("is-live", true)
        .build()
        .expect("audiotestsrc を作れない");
    let id = GStreamerMediaStream::try_create_audio_from_device(source, Some("test-id".to_owned()))
        .expect("デバイス付き音声トラックを作れない");
    let settings = stream_settings(&id).expect("音声トラックが登録されていない");
    assert_eq!(
        settings.device_id.as_deref(),
        Some("test-id"),
        "選択デバイスの ID を settings で返す"
    );
    let clone = clone_stream(&id).expect("音声トラックを複製できない");
    let clone_settings = stream_settings(&clone).expect("複製トラックが登録されていない");
    assert_eq!(
        clone_settings.device_id.as_deref(),
        Some("test-id"),
        "複製枝でも同じ device_id を返す"
    );
    stop_stream(&id).expect("元トラックを停止できない");
    stop_stream(&clone).expect("複製トラックを停止できない");
}

#[test]
fn clone_mute_and_stop_only_affect_that_audio_branch() {
    let original = TestTrack::audio();
    let clone = original.clone();
    let source = original.source();

    set_stream_enabled(&clone.0, false).expect("複製トラックをミュートできない");
    let original_output = TestConsumer::attach(&original);
    let clone_output = TestConsumer::attach(&clone);

    assert!(
        original_output.pull_bytes().iter().any(|byte| *byte != 0),
        "複製をミュートしても元トラックは合成音声を出力する"
    );
    let settings = stream_settings(&original.0).expect("元トラックが登録されていない");
    assert_eq!(
        settings.sample_rate,
        Some(44_100),
        "合成音声の確定済み caps から sampleRate を返す"
    );
    assert!(
        clone_output.pull_until_silent(),
        "ミュートした複製トラックは無音を出力する"
    );

    set_stream_enabled(&clone.0, true).expect("複製トラックのミュートを解除できない");
    assert!(
        clone_output.pull_bytes().iter().any(|byte| *byte != 0),
        "複製トラックだけを有効に戻すと音声が再開する"
    );
    assert!(
        original_output.pull_bytes().iter().any(|byte| *byte != 0),
        "複製トラックの有効化は元トラックの出力を変えない"
    );

    stop_stream(&original.0).expect("元トラックを停止できない");
    assert!(
        clone_stream(&original.0).is_err(),
        "停止した枝から共有入力へ新しい枝を追加できない"
    );
    assert_eq!(
        source.lock().unwrap().pipeline.current_state(),
        gstreamer::State::Playing,
        "元トラック停止後も複製トラックが共有入力を動かす"
    );
    assert!(
        clone_output.pull_bytes().iter().any(|byte| *byte != 0),
        "元トラック停止後も複製トラックは合成音声を出力する"
    );

    stop_stream(&clone.0).expect("最後の複製トラックを停止できない");
    stop_stream(&clone.0).expect("停止済み枝の繰り返し停止は成功する");
    assert_eq!(
        source.lock().unwrap().pipeline.current_state(),
        gstreamer::State::Null,
        "最後の枝を停止すると共有入力パイプラインが終了する"
    );
}

#[test]
fn unread_clone_does_not_block_audio_from_the_original() {
    let original = TestTrack::audio();
    let unread_clone = original.clone();
    let output = TestConsumer::attach(&original);

    // 枝のキュー上限を越えてから読むことで、未接続の複製枝による逆圧を検出する。
    thread::sleep(Duration::from_millis(750));
    assert!(
        output.pull_bytes().iter().any(|byte| *byte != 0),
        "読み手のない複製枝があっても元トラックの音声が流れ続ける"
    );

    stop_stream(&unread_clone.0).expect("未読の複製トラックを停止できない");
    assert!(
        output.pull_bytes().iter().any(|byte| *byte != 0),
        "未読の複製停止後も元トラックは音声を出力する"
    );
}

#[test]
fn attached_audio_track_can_be_published_from_a_fresh_shared_branch() {
    let (original, destination_input) = TestTrack::audio_destination();
    let source = original.source();
    let original_output = TestConsumer::attach(&original);
    let original_pipeline = {
        let stream = get_stream(&original.0).expect("元トラックが登録されていない");
        let stream = stream.lock().unwrap();
        stream
            .as_any()
            .downcast_ref::<GStreamerMediaStream>()
            .and_then(GStreamerMediaStream::attached_pipeline)
            .expect("元トラックの読み取り pipeline がない")
    };
    assert_eq!(
        original_output.pipeline.as_ptr(),
        original_pipeline.as_ptr()
    );

    let clone_id = {
        let stream = get_stream(&original.0).expect("元トラックが登録されていない");
        let stream = stream.lock().unwrap();
        stream
            .as_any()
            .downcast_ref::<GStreamerMediaStream>()
            .expect("元トラックが GStreamer のストリームでない")
            .try_clone_branch()
            .expect("別 pipeline 用の共有 source 枝を作れない")
    };
    let clone = TestTrack(clone_id);
    let clone_before_attach = get_stream(&clone.0).expect("複製トラックが登録されていない");
    assert!(
        clone_before_attach
            .lock()
            .unwrap()
            .as_any()
            .downcast_ref::<GStreamerMediaStream>()
            .and_then(GStreamerMediaStream::attached_pipeline)
            .is_none(),
        "新しい枝は既存 consumer の pipeline を共有しない"
    );

    let clone_output = TestConsumer::attach(&clone);
    assert_ne!(
        original_output.pipeline.as_ptr(),
        clone_output.pipeline.as_ptr(),
        "各 consumer は独立した pipeline に接続される"
    );
    assert!(
        original_output.pull_bytes().iter().any(|byte| *byte != 0),
        "元 consumer は合成音声を受信する"
    );
    assert!(
        clone_output.pull_bytes().iter().any(|byte| *byte != 0),
        "新しい共有 source 枝も合成音声を受信する"
    );

    set_stream_enabled(&original.0, false).expect("元トラックをミュートできない");
    assert!(
        original_output.pull_until_silent(),
        "元トラックをミュートすると元 consumer は無音になる"
    );
    assert!(
        clone_output.pull_until_silent(),
        "元トラックをミュートすると WebRTC 用の共有制御枝も無音になる"
    );
    set_stream_enabled(&original.0, true).expect("元トラックのミュートを解除できない");
    assert!(
        clone_output.pull_bytes().iter().any(|byte| *byte != 0),
        "元トラックのミュート解除が WebRTC 用枝にも反映される"
    );

    stop_stream(&original.0).expect("元トラックを停止できない");
    assert_eq!(
        source.lock().unwrap().pipeline.current_state(),
        gstreamer::State::Playing,
        "元 consumer を止めても複製枝の共有入力は動作する"
    );
    assert!(
        clone_output.pull_until_silent(),
        "元トラック停止後、WebRTC 用の共有制御枝は無音になる"
    );
    stop_stream(&clone.0).expect("複製トラックを停止できない");
    assert_eq!(
        source.lock().unwrap().pipeline.current_state(),
        gstreamer::State::Null,
        "最後の枝を停止すると共有入力が終了する"
    );
    drop(destination_input);
}

#[test]
fn encoded_for_playback_reuses_one_tee_for_multiple_consumers() {
    gstreamer::init().expect("GStreamer must initialize for playback output tests");
    let stream_id = GStreamerMediaStream::create_video();
    {
        let stream = get_stream(&stream_id).expect("the video track should be registered");
        let mut stream = stream.lock().unwrap();
        let stream = stream
            .as_mut_any()
            .downcast_mut::<GStreamerMediaStream>()
            .expect("the video track should use the GStreamer backend");
        let (tee1, pipeline1) = stream
            .encoded_for_playback()
            .expect("the first playback output should be created");
        let (tee2, pipeline2) = stream
            .encoded_for_playback()
            .expect("the second playback output should reuse the tee");
        assert!(
            tee1 == tee2,
            "encoded_for_playback must reuse the same tee for the same stream"
        );
        assert_eq!(
            pipeline1.name(),
            pipeline2.name(),
            "the second playback output must reuse the same pipeline"
        );
        assert!(
            stream.playback_clone.is_none(),
            "an unlinked source branch must not be cloned for playback"
        );

        let pipeline = pipeline1;
        let sink1 = gstreamer::ElementFactory::make("proxysink")
            .build()
            .expect("the first proxysink should be available");
        let sink2 = gstreamer::ElementFactory::make("proxysink")
            .build()
            .expect("the second proxysink should be available");
        pipeline
            .add_many([&sink1, &sink2])
            .expect("proxysinks should be added to the pipeline");
        tee1
            .link(&sink1)
            .expect("the shared tee should link the first proxysink");
        tee1
            .link(&sink2)
            .expect("the shared tee should link the second proxysink");
    }
    let _ = stop_stream(&stream_id);
}

#[test]
fn encoded_for_playback_clones_branch_when_source_is_linked() {
    gstreamer::init().expect("GStreamer must initialize for playback output tests");
    let stream_id = GStreamerMediaStream::create_video();
    let (tee, pipeline, clone_id, shared) = {
        let stream = get_stream(&stream_id).expect("the video track should be registered");
        let mut stream = stream.lock().unwrap();
        let stream = stream
            .as_mut_any()
            .downcast_mut::<GStreamerMediaStream>()
            .expect("the video track should use the GStreamer backend");
        stream.pipeline_or_new();
        stream
            .encoded()
            .expect("the original encoder branch should be created");
        let (tee, pipeline) = stream
            .encoded_for_playback()
            .expect("the playback output should be created");
        let clone_id = stream
            .playback_clone
            .expect("a linked source must use a playback clone");
        let shared = stream
            .branch
            .as_ref()
            .expect("the track should have a branch")
            .source
            .clone();
        (tee, pipeline, clone_id, shared)
    };
    assert!(
        get_stream(&clone_id).is_some(),
        "the playback clone should remain registered while in use"
    );

    {
        let stream = get_stream(&stream_id).expect("the video track should be registered");
        let mut stream = stream.lock().unwrap();
        let stream = stream
            .as_mut_any()
            .downcast_mut::<GStreamerMediaStream>()
            .expect("the video track should use the GStreamer backend");
        let (tee2, pipeline2) = stream
            .encoded_for_playback()
            .expect("the playback output should be reused");
        assert!(tee == tee2, "the clone playback tee must be reused");
        assert_eq!(
            pipeline.name(),
            pipeline2.name(),
            "the clone playback pipeline must be reused"
        );
        assert!(
            stream.playback_clone == Some(clone_id),
            "only one playback clone should be created"
        );
    }

    let _ = stop_stream(&stream_id);
    assert!(
        get_stream(&clone_id).is_none(),
        "stopping the original stream must unregister the playback clone"
    );
    assert_eq!(
        shared.lock().unwrap().branches,
        0,
        "stopping the original and its playback clone must finish the shared source"
    );
}
