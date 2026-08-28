//! Frame accumulation and the bounded hand-off into the recognizer thread.
//!
//! The cpal callback delivers whatever buffer size the device feels like;
//! the recognizer wants ~100 ms frames. [`FrameAssembler`] bridges the two
//! without blocking: it fills a pre-allocated buffer and, when that buffer is
//! full, `try_send`s it. If the channel is full the *new* frame is dropped and
//! a counter is incremented, because dropped audio mid-utterance should be
//! surfaced rather than silently smoothed over.
//!
//! It owns nothing from cpal, so tests drive it directly.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc::{SyncSender, TrySendError},
};

use crate::recognition::AudioMsg;

/// Accumulates resampled mono PCM into fixed-size frames and pushes them onto
/// the bounded audio channel.
pub struct FrameAssembler {
    frame_len: usize,
    buffer: Vec<i16>,
    frames: SyncSender<AudioMsg>,
    dropped: Arc<AtomicU64>,
    /// Set once the recognizer has gone away, so we stop allocating frames
    /// nobody will ever read.
    closed: bool,
}

impl FrameAssembler {
    /// Creates an assembler emitting frames of `frame_len` samples. A length
    /// of zero is treated as one.
    pub fn new(frame_len: usize, frames: SyncSender<AudioMsg>, dropped: Arc<AtomicU64>) -> Self {
        let frame_len = frame_len.max(1);

        Self {
            frame_len,
            buffer: Vec::with_capacity(frame_len),
            frames,
            dropped,
            closed: false,
        }
    }

    /// The number of samples in each emitted frame.
    pub fn frame_len(&self) -> usize {
        self.frame_len
    }

    /// Whether the recognizer end of the channel has been dropped.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Appends `samples`, emitting frames as they fill up.
    pub fn push(&mut self, mut samples: &[i16]) {
        while !samples.is_empty() {
            let wanted = self.frame_len - self.buffer.len();
            let taken = wanted.min(samples.len());

            self.buffer.extend_from_slice(&samples[..taken]);
            samples = &samples[taken..];

            if self.buffer.len() == self.frame_len {
                self.emit();
            }
        }
    }

    /// Discards the partially filled frame without sending it.
    pub fn reset(&mut self) {
        self.buffer.clear();
    }

    fn emit(&mut self) {
        if self.closed {
            self.buffer.clear();
            return;
        }

        // The channel takes ownership of the samples, so a fresh buffer has to
        // be handed to the callback. That is one bounded allocation per frame
        // (ten per second), which is the price of the `Vec<i16>` contract.
        let frame = std::mem::replace(&mut self.buffer, Vec::with_capacity(self.frame_len));

        match self.frames.try_send(AudioMsg::Frame(frame)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.closed = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{TryRecvError, sync_channel};

    fn assembler(frame_len: usize, capacity: usize) -> (FrameAssembler, Harness) {
        let (tx, rx) = sync_channel(capacity);
        let dropped = Arc::new(AtomicU64::new(0));

        (
            FrameAssembler::new(frame_len, tx, dropped.clone()),
            Harness { rx, dropped },
        )
    }

    struct Harness {
        rx: std::sync::mpsc::Receiver<AudioMsg>,
        dropped: Arc<AtomicU64>,
    }

    impl Harness {
        fn frames(&self) -> Vec<Vec<i16>> {
            let mut out = Vec::new();
            loop {
                match self.rx.try_recv() {
                    Ok(AudioMsg::Frame(frame)) => out.push(frame),
                    Ok(AudioMsg::Reset | AudioMsg::Clear) => {
                        panic!("the assembler must never send a control message")
                    }
                    Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return out,
                }
            }
        }

        fn dropped(&self) -> u64 {
            self.dropped.load(Ordering::Relaxed)
        }
    }

    #[test]
    fn a_partial_frame_is_not_sent() {
        let (mut assembler, harness) = assembler(4, 8);
        assembler.push(&[1, 2, 3]);

        assert!(harness.frames().is_empty());
        assert_eq!(harness.dropped(), 0);
    }

    #[test]
    fn a_full_frame_is_sent() {
        let (mut assembler, harness) = assembler(4, 8);
        assembler.push(&[1, 2, 3, 4]);

        assert_eq!(harness.frames(), vec![vec![1, 2, 3, 4]]);
    }

    #[test]
    fn frames_accumulate_across_pushes() {
        let (mut assembler, harness) = assembler(4, 8);
        assembler.push(&[1, 2]);
        assembler.push(&[3]);
        assembler.push(&[4, 5, 6]);

        assert_eq!(harness.frames(), vec![vec![1, 2, 3, 4]]);
    }

    #[test]
    fn a_single_push_can_produce_several_frames() {
        let (mut assembler, harness) = assembler(2, 8);
        assembler.push(&[1, 2, 3, 4, 5, 6, 7]);

        assert_eq!(
            harness.frames(),
            vec![vec![1, 2], vec![3, 4], vec![5, 6]],
            "the trailing sample stays buffered"
        );
    }

    #[test]
    fn a_full_channel_drops_the_new_frame_and_counts_it() {
        let (mut assembler, harness) = assembler(2, 2);

        // Two frames fit in the channel; the third and fourth do not.
        assembler.push(&[1, 2, 3, 4, 5, 6, 7, 8]);

        assert_eq!(harness.dropped(), 2);
        assert_eq!(
            harness.frames(),
            vec![vec![1, 2], vec![3, 4]],
            "the frames already queued are the *older* ones"
        );
    }

    #[test]
    fn draining_the_channel_lets_capture_resume() {
        let (mut assembler, harness) = assembler(2, 1);

        assembler.push(&[1, 2, 3, 4]);
        assert_eq!(harness.dropped(), 1);
        assert_eq!(harness.frames(), vec![vec![1, 2]]);

        assembler.push(&[5, 6]);
        assert_eq!(harness.dropped(), 1);
        assert_eq!(harness.frames(), vec![vec![5, 6]]);
    }

    #[test]
    fn resetting_discards_the_partial_frame() {
        let (mut assembler, harness) = assembler(4, 8);

        assembler.push(&[1, 2, 3]);
        assembler.reset();
        assembler.push(&[9, 9, 9, 9]);

        assert_eq!(harness.frames(), vec![vec![9, 9, 9, 9]]);
    }

    #[test]
    fn a_disconnected_channel_stops_the_assembler() {
        let (mut assembler, harness) = assembler(2, 4);
        drop(harness);

        assembler.push(&[1, 2]);
        assert!(assembler.is_closed());

        // Subsequent pushes are cheap no-ops rather than repeated failures.
        assembler.push(&[3, 4, 5, 6]);
        assert!(assembler.is_closed());
    }

    #[test]
    fn a_zero_length_frame_is_clamped_to_one_sample() {
        let (mut assembler, harness) = assembler(0, 4);
        assert_eq!(assembler.frame_len(), 1);

        assembler.push(&[42]);
        assert_eq!(harness.frames(), vec![vec![42]]);
    }
}
