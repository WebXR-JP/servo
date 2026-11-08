/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use servo_media_streams::MediaSocket;

use crate::audio_node::{AudioNodeEngine, AudioNodeType, BlockInfo, ChannelInfo};
use crate::block::Chunk;
use crate::sink::AudioSink;

/// 送信側ダンプのファイル連番。
static DEST_DUMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(AudioNodeCommon)]
pub(crate) struct MediaStreamDestinationNode {
    channel_info: ChannelInfo,
    sink: Box<dyn AudioSink + 'static>,
    /// `XRIFT_AUDIO_DUMP` 指定時の送信音声の書き出し先。診断用。
    dump: Option<BufWriter<File>>,
    dump_disabled: bool,
}

impl MediaStreamDestinationNode {
    pub fn new(
        socket: Box<dyn MediaSocket>,
        sample_rate: f32,
        sink: Box<dyn AudioSink + 'static>,
        channel_info: ChannelInfo,
    ) -> Self {
        sink.init_stream(channel_info.count, sample_rate, socket)
            .expect("init_stream failed");
        sink.play().expect("Sink didn't start");
        MediaStreamDestinationNode {
            channel_info,
            sink,
            dump: None,
            dump_disabled: false,
        }
    }

    /// 送信ブロックを `<dir>/dest-<n>-<rate>ch<ch>.f32` へ raw little-endian f32 で追記する。
    fn dump_chunk(&mut self, chunk: &Chunk, info: &BlockInfo) {
        if self.dump_disabled {
            return;
        }
        if self.dump.is_none() {
            let Some(dir) = std::env::var_os("XRIFT_AUDIO_DUMP") else {
                self.dump_disabled = true;
                return;
            };
            let dir = std::path::PathBuf::from(dir);
            let seq = DEST_DUMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let rate = info.sample_rate as u32;
            let channels = self.channel_info.computed_number_of_channels();
            let path = dir.join(format!("dest-{seq}-{rate}ch{channels}.f32"));
            match File::create(&path) {
                Ok(file) => self.dump = Some(BufWriter::new(file)),
                Err(error) => {
                    log::warn!("送信音声ダンプを開けない ({}): {error}", path.display());
                    self.dump_disabled = true;
                    return;
                },
            }
        }
        let Some(writer) = self.dump.as_mut() else {
            return;
        };
        let mut failed: Option<std::io::Error> = None;
        'outer: for block in chunk.blocks.iter() {
            let mut snapshot = block.clone();
        // ダンプは常に channels * 128 の planar 配置にする（repeat を展開）。
        snapshot.explicit_repeat();
            for sample in snapshot.data_mut().iter() {
                if let Err(error) = writer.write_all(&sample.to_le_bytes()) {
                    failed = Some(error);
                    break 'outer;
                }
            }
        }
        if let Some(error) = failed {
            log::warn!("送信音声ダンプに書けない: {error}");
            self.dump = None;
            self.dump_disabled = true;
        }
    }
}

impl AudioNodeEngine for MediaStreamDestinationNode {
    fn node_type(&self) -> AudioNodeType {
        AudioNodeType::MediaStreamDestinationNode
    }

    fn process(&mut self, inputs: Chunk, info: &BlockInfo) -> Chunk {
        self.dump_chunk(&inputs, info);
        self.sink
            .push_data(inputs)
            .expect("Pushing to stream failed");
        Chunk::default()
    }

    fn output_count(&self) -> u32 {
        0
    }
}
