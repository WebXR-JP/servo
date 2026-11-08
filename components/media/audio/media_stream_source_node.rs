/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::AudioStreamReader;
use crate::audio_node::{AudioNodeEngine, AudioNodeType, BlockInfo, ChannelInfo};
use crate::block::{Block, Chunk};
use crate::param::{Param, ParamType};

/// 受信音声ダンプのファイル連番。プロセス内で一意にするための採番専用。
static DUMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(AudioNodeCommon)]
pub(crate) struct MediaStreamSourceNode {
    channel_info: ChannelInfo,
    reader: Box<dyn AudioStreamReader + Send>,
    playing: bool,
    /// `XRIFT_AUDIO_DUMP` 指定時の受信音声の書き出し先。初回の `process` で開く。
    /// 診断用のため、開けない・書けない場合は警告を1回だけ出して以降書かない。
    dump: Option<BufWriter<File>>,
    /// 真のときダンプを試さない。未設定時と失敗後で共通の抑止印。
    dump_disabled: bool,
}

impl MediaStreamSourceNode {
    pub fn new(reader: Box<dyn AudioStreamReader + Send>, channel_info: ChannelInfo) -> Self {
        Self {
            channel_info,
            reader,
            playing: false,
            dump: None,
            dump_disabled: false,
        }
    }

    /// 受信ブロックを `<dir>/stream-<n>-<rate>ch<ch>.f32` へ raw little-endian f32 で追記する。
    ///
    /// 出力対象のブロック自体は変えない（無音ブロックの扱いを含む既存の挙動を保つため、
    /// 複製側の `data_mut` で読み出す）。`XRIFT_AUDIO_DUMP` が未設定のときは何もしない。
    fn dump_block(&mut self, block: &Block, info: &BlockInfo) {
        if self.dump_disabled {
            return;
        }
        if self.dump.is_none() {
            let Some(dir) = std::env::var_os("XRIFT_AUDIO_DUMP") else {
                self.dump_disabled = true;
                return;
            };
            let dir = std::path::PathBuf::from(dir);
            let seq = DUMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let rate = info.sample_rate as u32;
            let channels = block.chan_count();
            let path = dir.join(format!("stream-{seq}-{rate}ch{channels}.f32"));
            match File::create(&path) {
                Ok(file) => {
                    self.dump = Some(BufWriter::new(file));
                }
                Err(error) => {
                    log::warn!(
                        "受信音声ダンプを開けない ({}): {error}",
                        path.display()
                    );
                    self.dump_disabled = true;
                    return;
                }
            }
        }
        let Some(writer) = self.dump.as_mut() else {
            return;
        };
        // 出力ブロックの無音表現（空バッファ・repeat）を変えないよう、複製側で読む。
        let mut snapshot = block.clone();
        // ダンプは常に channels * 128 の planar 配置にする（repeat を展開）。
        snapshot.explicit_repeat();
        let mut failed: Option<std::io::Error> = None;
        for sample in snapshot.data_mut().iter() {
            if let Err(error) = writer.write_all(&sample.to_le_bytes()) {
                failed = Some(error);
                break;
            }
        }
        if let Some(error) = failed {
            log::warn!("受信音声ダンプに書けない: {error}");
            self.dump = None;
            self.dump_disabled = true;
        }
    }
}

impl AudioNodeEngine for MediaStreamSourceNode {
    fn node_type(&self) -> AudioNodeType {
        AudioNodeType::MediaStreamSourceNode
    }

    fn process(&mut self, mut inputs: Chunk, info: &BlockInfo) -> Chunk {
        debug_assert!(inputs.is_empty());

        if !self.playing {
            self.playing = true;
            self.reader.start();
        }

        let block = self.reader.pull().unwrap_or_else(|| {
            // トラックがデータを出さない間は無音で埋める。ここでブロックすると
            // createMediaStreamSource の同期待ちでページのJSスレッドまで止まる。
            Block::for_channels_explicit(self.channel_info.computed_number_of_channels())
        });
        self.dump_block(&block, info);
        inputs.blocks.push(block);

        inputs
    }

    fn input_count(&self) -> u32 {
        0
    }

    fn get_param(&mut self, _: ParamType) -> &mut Param {
        panic!("No params on MediaStreamSourceNode");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::Tick;

    struct StubReader {
        block: Option<Block>,
    }

    impl AudioStreamReader for StubReader {
        fn pull(&self) -> Option<Block> {
            self.block.clone()
        }

        fn start(&self) {}

        fn stop(&self) {}
    }

    fn block_info() -> BlockInfo {
        BlockInfo {
            sample_rate: 44100.0,
            frame: Tick(0),
            time: 0.0,
        }
    }

    #[test]
    fn pull_none_は無音ブロックで埋める() {
        // データが無い間もレンダースレッドを止めず無音を出すことを確かめる。
        let reader = Box::new(StubReader { block: None });
        let channel_info = ChannelInfo::default();
        let expected_channels = channel_info.computed_number_of_channels();
        let mut node = MediaStreamSourceNode::new(reader, channel_info);

        let chunk = node.process(Chunk::default(), &block_info());

        assert_eq!(chunk.blocks.len(), 1);
        let block = &chunk.blocks[0];
        assert_eq!(block.chan_count(), expected_channels);
        let mut block = block.clone();
        assert!(block.data_mut().iter().all(|&sample| sample == 0.0));
    }

    #[test]
    fn pull_some_はそのまま出力する() {
        // データがある場合は無音化せずそのまま流すことを確かめる。
        let mut block = Block::for_channels_explicit(2);
        block.data_mut()[0] = 1.0;
        let reader = Box::new(StubReader {
            block: Some(block.clone()),
        });
        let mut node = MediaStreamSourceNode::new(reader, ChannelInfo::default());

        let chunk = node.process(Chunk::default(), &block_info());

        assert_eq!(chunk.blocks.len(), 1);
        assert_eq!(chunk.blocks[0].chan_count(), 2);
        let mut out = chunk.blocks[0].clone();
        assert_eq!(out.data_mut()[0], 1.0);
    }
}
