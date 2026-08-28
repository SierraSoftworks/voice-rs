//! The whole conversion path, assembled: interleaved device samples in,
//! ~100 ms frames of 16 kHz mono PCM out.
//!
//! This is the body of the cpal callback minus cpal itself, which means the
//! realtime path can be exercised end to end in a unit test.

use crate::audio::{
    convert::{IntoPcm16, downmix_to_mono},
    frames::FrameAssembler,
    resample::Resampler,
};

/// Converts one device buffer at a time into recognizer frames.
///
/// Both scratch buffers are reused across callbacks, so after the first few
/// buffers the only allocation left on the path is the frame handed to the
/// channel.
pub struct CapturePipeline {
    channels: u16,
    mono: Vec<i16>,
    resampled: Vec<i16>,
    resampler: Resampler,
    assembler: FrameAssembler,
}

impl CapturePipeline {
    /// Builds a pipeline for a device running at `input_rate` with `channels`
    /// interleaved channels, producing frames at `target_rate`.
    pub fn new(
        channels: u16,
        input_rate: u32,
        target_rate: u32,
        assembler: FrameAssembler,
    ) -> Self {
        Self {
            channels: channels.max(1),
            mono: Vec::new(),
            resampled: Vec::new(),
            resampler: Resampler::new(input_rate, target_rate),
            assembler,
        }
    }

    /// Processes one interleaved device buffer.
    pub fn push<T: IntoPcm16>(&mut self, interleaved: &[T]) {
        self.mono.clear();
        downmix_to_mono(interleaved, self.channels, &mut self.mono);

        self.resampled.clear();
        self.resampler.process(&self.mono, &mut self.resampled);

        self.assembler.push(&self.resampled);
    }

    /// Throws away every scrap of buffered state. Called while listening is
    /// off so that audio from before a mute can never be stitched onto audio
    /// from after it.
    pub fn discard(&mut self) {
        self.mono.clear();
        self.resampled.clear();
        self.resampler.reset();
        self.assembler.reset();
    }

    /// Whether the recognizer end of the channel has gone away.
    pub fn is_closed(&self) -> bool {
        self.assembler.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recognition::AudioMsg;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, TryRecvError, sync_channel},
    };

    struct Harness {
        pipeline: CapturePipeline,
        rx: Receiver<AudioMsg>,
        dropped: Arc<AtomicU64>,
    }

    impl Harness {
        fn new(channels: u16, input_rate: u32, target_rate: u32, capacity: usize) -> Self {
            let (tx, rx) = sync_channel(capacity);
            let dropped = Arc::new(AtomicU64::new(0));
            let frame_len = (target_rate / 10) as usize;

            Self {
                pipeline: CapturePipeline::new(
                    channels,
                    input_rate,
                    target_rate,
                    FrameAssembler::new(frame_len, tx, dropped.clone()),
                ),
                rx,
                dropped,
            }
        }

        fn frames(&self) -> Vec<Vec<i16>> {
            let mut out = Vec::new();
            loop {
                match self.rx.try_recv() {
                    Ok(AudioMsg::Frame(frame)) => out.push(frame),
                    Ok(AudioMsg::Reset | AudioMsg::Clear) => {
                        panic!("the pipeline must never send a control message")
                    }
                    Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return out,
                }
            }
        }

        fn dropped(&self) -> u64 {
            self.dropped.load(Ordering::Relaxed)
        }
    }

    fn stereo_f32(frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let phase = i as f32 / 200.0 * std::f32::consts::TAU;
                [phase.sin() * 0.5, phase.sin() * 0.5]
            })
            .collect()
    }

    #[test]
    fn a_48k_stereo_f32_device_yields_16k_mono_frames() {
        // One second of audio at 48 kHz should produce ten 100 ms frames.
        let mut harness = Harness::new(2, 48_000, 16_000, 64);

        for chunk in stereo_f32(48_000).chunks(960) {
            harness.pipeline.push(chunk);
        }

        let frames = harness.frames();
        assert_eq!(frames.len(), 10);
        assert!(frames.iter().all(|f| f.len() == 1_600));
        assert_eq!(harness.dropped(), 0);
    }

    #[test]
    fn a_44k1_mono_i16_device_yields_16k_mono_frames() {
        let mut harness = Harness::new(1, 44_100, 16_000, 64);

        let input: Vec<i16> = (0..44_100).map(|i| ((i % 400) as i16) * 40).collect();
        for chunk in input.chunks(441) {
            harness.pipeline.push(chunk);
        }

        let frames = harness.frames();
        assert_eq!(frames.len(), 10);
        assert!(frames.iter().all(|f| f.len() == 1_600));
    }

    #[test]
    fn a_16k_mono_i16_device_passes_straight_through() {
        let mut harness = Harness::new(1, 16_000, 16_000, 64);

        let input: Vec<i16> = (0..3_200).map(|i| i as i16).collect();
        harness.pipeline.push(&input);

        let frames = harness.frames();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], input[..1_600]);
        assert_eq!(frames[1], input[1_600..]);
    }

    #[test]
    fn a_backed_up_channel_counts_dropped_frames() {
        let mut harness = Harness::new(1, 16_000, 16_000, 2);

        harness.pipeline.push(&vec![0i16; 1_600 * 5]);

        assert_eq!(harness.dropped(), 3);
        assert_eq!(harness.frames().len(), 2);
    }

    #[test]
    fn discarding_clears_the_partial_frame() {
        let mut harness = Harness::new(1, 16_000, 16_000, 8);

        harness.pipeline.push(&vec![7i16; 800]);
        harness.pipeline.discard();
        harness.pipeline.push(&vec![9i16; 1_600]);

        let frames = harness.frames();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].iter().all(|&s| s == 9));
    }

    #[test]
    fn a_u16_device_is_recentred_before_framing() {
        let mut harness = Harness::new(1, 16_000, 16_000, 8);

        harness.pipeline.push(&vec![32_768u16; 1_600]);

        assert_eq!(harness.frames(), vec![vec![0i16; 1_600]]);
    }

    #[test]
    fn a_closed_channel_is_reported() {
        let (tx, rx) = sync_channel(4);
        drop(rx);

        let mut pipeline = CapturePipeline::new(
            1,
            16_000,
            16_000,
            FrameAssembler::new(1_600, tx, Arc::new(AtomicU64::new(0))),
        );

        assert!(!pipeline.is_closed());
        pipeline.push(&vec![0i16; 1_600]);
        assert!(pipeline.is_closed());
    }
}
