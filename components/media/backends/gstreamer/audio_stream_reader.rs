/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError, channel};
use std::time::Duration;

use byte_slice_cast::*;
use gstreamer::Fraction;
use gstreamer::prelude::*;
use gstreamer_audio::AUDIO_FORMAT_F32;
use servo_media_audio::AudioStreamReader;
use servo_media_audio::block::{Block, FRAMES_PER_BLOCK_USIZE};
use servo_media_streams::registry::{MediaStreamId, get_stream, stop_stream, unregister_stream};

use crate::media_stream::GStreamerMediaStream;

// audiobuffersplit は既定 strict-buffer-size=false のため、EOS などで 128 フレームに
// 満たない端数バッファを流す。そのまま Block::for_vec に渡すと 128 の倍数前提の
// assertion で panic するため、ここで 128 フレーム分が溜まるまで蓄積する。
struct BlockAssembler {
    channels: Option<usize>,
    pending: Vec<f32>,
    blocks: Vec<Block>,
}

impl BlockAssembler {
    fn new() -> Self {
        Self {
            channels: None,
            pending: Vec::new(),
            blocks: Vec::new(),
        }
    }

    /// 受け取ったサンプルを蓄積し、完成した 128 フレームのブロックを返す。
    /// 端数は次回に繰り越す。
    fn push(&mut self, samples: &[f32], channels: usize) -> std::vec::Drain<'_, Block> {
        self.blocks.clear();
        if channels == 0 || channels > u8::MAX as usize {
            self.pending.clear();
            return self.blocks.drain(..);
        }
        if self.channels != Some(channels) {
            self.pending.clear();
            self.channels = Some(channels);
        }
        self.pending.extend_from_slice(samples);
        let block_len = FRAMES_PER_BLOCK_USIZE * channels;
        while self.pending.len() >= block_len {
            let remainder = self.pending.split_off(block_len);
            let samples = std::mem::replace(&mut self.pending, remainder);
            self.blocks.push(Block::for_vec(samples));
        }
        self.blocks.drain(..)
    }
}

/// 音声ブロックの受信。データが途切れたらブロックせず None を返す。
struct BlockReceiver {
    starved: AtomicBool,
    label: String,
}

impl BlockReceiver {
    fn new(label: String) -> Self {
        Self {
            starved: AtomicBool::new(false),
            label,
        }
    }

    /// 次のブロックを待つ。一定時間データが来なければ None を返し、
    /// 以降はデータが戻るまで待たずに None を返し続ける。
    fn recv(&self, rx: &Receiver<Block>) -> Option<Block> {
        // タイムアウト値を毎回待つとグラフ全体が 20ms/ブロックに絞られてしまうため、
        // 一度途切れたらデータが戻るまでブロックしない。
        const PULL_TIMEOUT: Duration = Duration::from_millis(20);
        // 一度途切れたら、データが戻るまでブロックしない。ブロックしたままレンダースレッドを
        // 止めると、createMediaStreamSource を同期で待つページのJSスレッドまで固まる。
        if self.starved.load(Ordering::Acquire) {
            return match rx.try_recv() {
                Ok(block) => {
                    self.starved.store(false, Ordering::Release);
                    log::info!("音声ストリーム {} のデータが戻った", self.label);
                    Some(block)
                },
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => None,
            };
        }
        match rx.recv_timeout(PULL_TIMEOUT) {
            Ok(block) => Some(block),
            Err(RecvTimeoutError::Timeout) => {
                self.starved.store(true, Ordering::Release);
                log::warn!(
                    "音声ストリーム {} が {} ms データを出さないため無音で埋める",
                    self.label,
                    PULL_TIMEOUT.as_millis()
                );
                None
            },
            Err(RecvTimeoutError::Disconnected) => {
                self.starved.store(true, Ordering::Release);
                log::warn!("音声ストリーム {} が終了した", self.label);
                None
            },
        }
    }
}

pub struct GStreamerAudioStreamReader {
    rx: Receiver<Block>,
    pipeline: gstreamer::Pipeline,
    source_element: gstreamer::Element,
    reader_elements: Vec<gstreamer::Element>,
    reader_stream: Option<MediaStreamId>,
    owns_pipeline: bool,
    stopped: AtomicBool,
    receiver: BlockReceiver,
}

struct ReaderBranchGuard {
    stream: Option<MediaStreamId>,
    pipeline: Option<gstreamer::Pipeline>,
}

impl Drop for ReaderBranchGuard {
    fn drop(&mut self) {
        if let Some(pipeline) = &self.pipeline {
            let _ = pipeline.set_state(gstreamer::State::Null);
        }
        if let Some(stream) = self.stream {
            let _ = stop_stream(&stream);
            unregister_stream(&stream);
        }
    }
}

impl ReaderBranchGuard {
    fn keep(mut self) -> MediaStreamId {
        self.pipeline = None;
        self.stream
            .take()
            .expect("reader branch guard contains a stream")
    }
}

impl GStreamerAudioStreamReader {
    pub fn new(stream: MediaStreamId, sample_rate: f32) -> Result<Self, String> {
        let (tx, rx) = channel();
        let original =
            get_stream(&stream).ok_or_else(|| String::from("音声ストリームが見つからない"))?;
        let (source_element, pipeline, owns_pipeline, branch_guard) = {
            let mut original = original.lock().unwrap();
            let g_stream = original
                .as_mut_any()
                .downcast_mut::<GStreamerMediaStream>()
                .ok_or_else(|| String::from("GStreamer 音声ストリームではない"))?;
            let original_source = g_stream.src_element();
            let source_pad = original_source
                .static_pad("src")
                .ok_or_else(|| String::from("音声ストリームに src pad がない"))?;

            // A WebAudio silence check can run while WebRTC already consumes the same track.
            // Give the reader its own tee branch so its appsink does not compete for one src pad.
            let cloned_stream = match g_stream.try_clone_branch() {
                Ok(stream) => Some(stream),
                Err(error) if source_pad.is_linked() => {
                    return Err(format!(
                        "接続済み音声ストリームの reader 枝を作れない: {error}"
                    ));
                },
                Err(_) if g_stream.attached_pipeline().is_none() => None,
                Err(error) => {
                    return Err(format!(
                        "既存 pipeline に接続済みの音声ストリームを reader 用に複製できない: {error}"
                    ));
                },
            };
            let reader_stream = cloned_stream.unwrap_or(stream);
            let mut branch_guard = cloned_stream.map(|stream| ReaderBranchGuard {
                stream: Some(stream),
                pipeline: None,
            });
            let owns_pipeline = cloned_stream.is_some() || g_stream.attached_pipeline().is_none();
            drop(original);

            let reader = get_stream(&reader_stream)
                .ok_or_else(|| String::from("音声 reader 用のストリームが見つからない"))?;
            let mut reader = reader.lock().unwrap();
            let reader = reader
                .as_mut_any()
                .downcast_mut::<GStreamerMediaStream>()
                .ok_or_else(|| String::from("音声 reader 用のストリームが GStreamer ではない"))?;
            let source_element = reader.src_element();
            let pipeline = reader.pipeline_or_new();
            if let Some(branch_guard) = &mut branch_guard {
                branch_guard.pipeline = Some(pipeline.clone());
            }
            (source_element, pipeline, owns_pipeline, branch_guard)
        };
        let time_per_block = Fraction::new(FRAMES_PER_BLOCK_USIZE as i32, sample_rate as i32);

        // XXXManishearth this is only necessary because of an upstream
        // gstreamer bug. https://github.com/servo/media/pull/362#issuecomment-647947034
        let caps = gstreamer_audio::AudioCapsBuilder::new()
            .layout(gstreamer_audio::AudioLayout::Interleaved)
            .build();
        let capsfilter0 = gstreamer::ElementFactory::make("capsfilter")
            .property("caps", caps)
            .build()
            .map_err(|error| format!("capsfilter creation failed: {error:?}"))?;

        let split = gstreamer::ElementFactory::make("audiobuffersplit")
            .property("output-buffer-duration", time_per_block)
            .build()
            .map_err(|error| format!("audiobuffersplit creation failed: {error:?}"))?;
        let convert = gstreamer::ElementFactory::make("audioconvert")
            .build()
            .map_err(|error| format!("audioconvert creation failed: {error:?}"))?;
        let caps = gstreamer_audio::AudioCapsBuilder::new()
            .layout(gstreamer_audio::AudioLayout::NonInterleaved)
            .format(AUDIO_FORMAT_F32)
            .rate(sample_rate as i32)
            .build();
        let capsfilter = gstreamer::ElementFactory::make("capsfilter")
            .property("caps", caps)
            .build()
            .map_err(|error| format!("capsfilter creation failed: {error:?}"))?;
        let sink = gstreamer::ElementFactory::make("appsink")
            .property("sync", false)
            .build()
            .map_err(|error| format!("appsink creation failed: {error:?}"))?;

        let appsink = sink
            .clone()
            .dynamic_cast::<gstreamer_app::AppSink>()
            .unwrap();

        let elements = [
            &source_element,
            &capsfilter0,
            &split,
            &convert,
            &capsfilter,
            &sink,
        ];
        pipeline
            .add_many(&elements[1..])
            .map_err(|error| format!("pipeline adding failed: {error:?}"))?;
        gstreamer::Element::link_many(elements)
            .map_err(|error| format!("element linking failed: {error:?}"))?;
        for e in &elements {
            e.sync_state_with_parent().map_err(|e| e.to_string())?;
        }
        let mut assembler = BlockAssembler::new();
        appsink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |appsink| {
                    let sample = appsink
                        .pull_sample()
                        .map_err(|_| gstreamer::FlowError::Eos)?;
                    let channels = sample
                        .caps()
                        .and_then(|caps| gstreamer_audio::AudioInfo::from_caps(caps).ok())
                        .map(|info| info.channels() as usize);
                    let Some(channels) = channels else {
                        return Ok(gstreamer::FlowSuccess::Ok);
                    };
                    let buffer = sample.buffer_owned().ok_or(gstreamer::FlowError::Error)?;

                    let buffer = buffer
                        .into_mapped_buffer_readable()
                        .map_err(|_| gstreamer::FlowError::Error)?;
                    let floatref = buffer
                        .as_slice()
                        .as_slice_of::<f32>()
                        .map_err(|_| gstreamer::FlowError::Error)?;

                    for block in assembler.push(floatref, channels) {
                        tx.send(block).map_err(|_| gstreamer::FlowError::Error)?;
                    }
                    Ok(gstreamer::FlowSuccess::Ok)
                })
                .build(),
        );
        let reader_stream = branch_guard.map(ReaderBranchGuard::keep);
        let stream_label = format!("{}", stream.id());
        log::info!(
            "音声 reader を開始: stream={} rate={} 独立pipeline={}",
            stream_label,
            sample_rate,
            owns_pipeline
        );
        Ok(Self {
            rx,
            pipeline,
            source_element: source_element.clone(),
            reader_elements: elements[1..]
                .iter()
                .map(|element| (*element).clone())
                .collect(),
            reader_stream,
            owns_pipeline,
            stopped: AtomicBool::new(false),
            receiver: BlockReceiver::new(stream_label),
        })
    }
}

impl GStreamerAudioStreamReader {
    fn stop_pipeline(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.owns_pipeline {
            let _ = self.pipeline.set_state(gstreamer::State::Null);
        } else {
            if let (Some(source_pad), Some(sink_pad)) = (
                self.source_element.static_pad("src"),
                self.reader_elements
                    .first()
                    .and_then(|element| element.static_pad("sink")),
            ) {
                let _ = source_pad.unlink(&sink_pad);
            }
            for element in self.reader_elements.iter().rev() {
                let _ = element.set_state(gstreamer::State::Null);
                if element.parent().is_some() {
                    let _ = self.pipeline.remove(element);
                }
            }
        }
        if let Some(stream) = self.reader_stream {
            let _ = stop_stream(&stream);
            unregister_stream(&stream);
        }
    }
}

impl Drop for GStreamerAudioStreamReader {
    fn drop(&mut self) {
        self.stop_pipeline();
    }
}

impl AudioStreamReader for GStreamerAudioStreamReader {
    fn pull(&self) -> Option<Block> {
        self.receiver.recv(&self.rx)
    }

    fn start(&self) {
        if self.stopped.load(Ordering::Acquire) {
            return;
        }
        self.pipeline.set_state(gstreamer::State::Playing).unwrap();
    }

    fn stop(&self) {
        // AudioContext suspend/resume can stop and restart the reader without ending its track.
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use servo_media_streams::registry::set_stream_enabled;

    fn receive_matching_audio(reader: &GStreamerAudioStreamReader, audible: bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let Ok(block) = reader.rx.recv_timeout(remaining) else {
                return false;
            };
            let has_audio = block.data_chan(0).iter().any(|sample| sample.abs() > 0.001);
            if has_audio == audible {
                return true;
            }
        }
    }

    #[test]
    fn reader_tracks_an_attached_stream_without_stealing_its_src_pad() {
        gstreamer::init().expect("GStreamer を初期化できない");
        let stream = GStreamerMediaStream::create_audio();
        let stream_handle = get_stream(&stream).expect("音声ストリームが登録されている");
        let pipeline = {
            let mut stream_handle = stream_handle.lock().unwrap();
            let stream_handle = stream_handle
                .as_mut_any()
                .downcast_mut::<GStreamerMediaStream>()
                .expect("GStreamer audio stream");
            let pipeline = stream_handle.pipeline_or_new();
            let source = stream_handle.src_element();
            let sink = gstreamer::ElementFactory::make("fakesink")
                .build()
                .expect("fakesink を作れる");
            pipeline
                .add(&sink)
                .expect("元 pipeline に fakesink を追加できる");
            source
                .link(&sink)
                .expect("元 stream を fakesink に接続できる");
            sink.sync_state_with_parent()
                .expect("元 stream の fakesink を開始できる");
            pipeline
        };
        pipeline
            .set_state(gstreamer::State::Playing)
            .expect("元 pipeline を開始できる");

        let (audible, muted, resumed, ended_silent, shared_pipeline_kept_playing, reader_stream) = {
            let reader = GStreamerAudioStreamReader::new(stream, 48_000.0)
                .expect("既接続 track から独立 reader 枝を作れる");
            let reader_stream = reader.reader_stream;
            reader.start();
            let audible = receive_matching_audio(&reader, true);

            set_stream_enabled(&stream, false).expect("元 track をミュートできる");
            let muted = receive_matching_audio(&reader, false);

            set_stream_enabled(&stream, true).expect("元 track のミュートを解除できる");
            let resumed = receive_matching_audio(&reader, true);

            stop_stream(&stream).expect("元 track を停止できる");
            let ended_silent = receive_matching_audio(&reader, false);

            reader.stop();
            let shared_pipeline_kept_playing =
                pipeline.current_state() == gstreamer::State::Playing;
            (
                audible,
                muted,
                resumed,
                ended_silent,
                shared_pipeline_kept_playing,
                reader_stream,
            )
        };
        let reader_branch_unregistered =
            reader_stream.is_none_or(|stream| get_stream(&stream).is_none());

        pipeline
            .set_state(gstreamer::State::Null)
            .expect("元 pipeline を停止できる");
        unregister_stream(&stream);

        assert!(audible, "既接続 track の reader が音声を受信する");
        assert!(muted, "元 track のミュートが reader に反映される");
        assert!(resumed, "元 track のミュート解除が reader に反映される");
        assert!(
            ended_silent,
            "元 track の終了が reader に無音として反映される"
        );
        assert!(
            shared_pipeline_kept_playing,
            "reader の停止が既存 pipeline を止めない"
        );
        assert!(reader_branch_unregistered, "reader 用 track 枝を破棄する");
    }
}

#[cfg(test)]
mod block_assembler_tests {
    use super::*;

    #[test]
    fn push_full_block_for_two_channels() {
        let mut assembler = BlockAssembler::new();
        let samples = vec![0.5; FRAMES_PER_BLOCK_USIZE * 2];
        let blocks: Vec<_> = assembler.push(&samples, 2).collect();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].data_chan(0).len(), FRAMES_PER_BLOCK_USIZE);
        assert_eq!(blocks[0].data_chan(1).len(), FRAMES_PER_BLOCK_USIZE);
        assert_eq!(blocks[0].chan_count(), 2);
    }

    #[test]
    fn push_carries_over_remainder() {
        let mut assembler = BlockAssembler::new();
        let first = vec![1.0; 300];
        assert_eq!(assembler.push(&first, 2).len(), 1);
        let second = vec![2.0; 212];
        let blocks: Vec<_> = assembler.push(&second, 2).collect();
        assert_eq!(blocks.len(), 1);
        // 300 + 212 = 512 = 2 ブロック分が端数の繰り越しで完成する。
        // 2 ブロック目は前回の端数 44 (1.0) と今回の 212 (2.0) の継ぎ目を含む。
        let chan0 = blocks[0].data_chan(0);
        assert!(chan0.contains(&1.0));
        assert!(chan0.contains(&2.0));
    }

    #[test]
    fn push_discards_pending_on_channel_change() {
        let mut assembler = BlockAssembler::new();
        // 2ch で端数 100 を溜める。
        assert_eq!(assembler.push(&vec![1.0; 100], 2).len(), 0);
        // 1ch に変わると端数は破棄され、1ch 基準でブロックが作られる。
        let blocks: Vec<_> = assembler
            .push(&vec![2.0; FRAMES_PER_BLOCK_USIZE], 1)
            .collect();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].chan_count(), 1);
        assert!(blocks[0].data_chan(0).iter().all(|&s| s == 2.0));
    }

    #[test]
    fn push_with_zero_channels_does_not_panic_and_recovers() {
        let mut assembler = BlockAssembler::new();
        assert_eq!(assembler.push(&vec![1.0; 256], 0).len(), 0);
        let blocks: Vec<_> = assembler
            .push(&vec![1.0; FRAMES_PER_BLOCK_USIZE * 2], 2)
            .collect();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].chan_count(), 2);
    }

    #[test]
    fn push_below_one_block_returns_nothing() {
        let mut assembler = BlockAssembler::new();
        assert_eq!(assembler.push(&vec![1.0; 100], 2).len(), 0);
    }
}

#[cfg(test)]
mod block_receiver_tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    #[test]
    fn empty_channel_returns_none_and_becomes_starved() {
        let (_tx, rx) = channel();
        let receiver = BlockReceiver::new(String::from("test"));
        assert!(receiver.recv(&rx).is_none());
        assert!(receiver.starved.load(Ordering::Acquire));
    }

    #[test]
    fn starved_recv_does_not_block() {
        let (_tx, rx) = channel();
        let receiver = BlockReceiver::new(String::from("test"));
        assert!(receiver.recv(&rx).is_none());
        let start = Instant::now();
        for _ in 0..50 {
            assert!(receiver.recv(&rx).is_none());
        }
        assert!(
            start.elapsed() < Duration::from_millis(250),
            "starved 中は待たずに None を返す"
        );
    }

    #[test]
    fn data_recovers_from_starved_and_waits_again() {
        let (tx, rx) = channel();
        let receiver = BlockReceiver::new(String::from("test"));
        assert!(receiver.recv(&rx).is_none());
        assert!(receiver.starved.load(Ordering::Acquire));
        tx.send(Block::for_channels_explicit(1)).unwrap();
        assert!(receiver.recv(&rx).is_some());
        assert!(!receiver.starved.load(Ordering::Acquire));
        assert!(receiver.recv(&rx).is_none());
        assert!(receiver.starved.load(Ordering::Acquire));
    }

    #[test]
    fn disconnected_sender_returns_none() {
        let (tx, rx) = channel::<Block>();
        drop(tx);
        let receiver = BlockReceiver::new(String::from("test"));
        assert!(receiver.recv(&rx).is_none());
    }
}
