/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use crate::AudioStreamReader;
use crate::audio_node::{AudioNodeEngine, AudioNodeType, BlockInfo, ChannelInfo};
use crate::block::{Block, Chunk};
use crate::param::{Param, ParamType};

#[derive(AudioNodeCommon)]
pub(crate) struct MediaStreamSourceNode {
    channel_info: ChannelInfo,
    reader: Box<dyn AudioStreamReader + Send>,
    playing: bool,
}

impl MediaStreamSourceNode {
    pub fn new(reader: Box<dyn AudioStreamReader + Send>, channel_info: ChannelInfo) -> Self {
        Self {
            channel_info,
            reader,
            playing: false,
        }
    }
}

impl AudioNodeEngine for MediaStreamSourceNode {
    fn node_type(&self) -> AudioNodeType {
        AudioNodeType::MediaStreamSourceNode
    }

    fn process(&mut self, mut inputs: Chunk, _: &BlockInfo) -> Chunk {
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
