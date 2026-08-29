//! Audio capture: cpal input stream, format conversion to the model's sample
//! rate in 16-bit mono PCM, and the bounded frame channel into the recognizer
//! thread. See DESIGN.md §"Audio capture (cpal)".
//!
//! The shape of this module follows the one rule the callback imposes: it runs
//! on a realtime thread, so it may not block and may not allocate without
//! bound. Everything it does is therefore split into small stateful structs
//! which the callback merely drives:
//!
//! ```text
//! cpal callback ─► CapturePipeline ─► downmix_to_mono ─► Resampler ─► FrameAssembler ─► try_send
//! ```
//!
//! Each of those pieces is a plain Rust type with no cpal in its signature, so
//! the conversion maths is unit-tested without a sound card; only device and
//! format selection need real hardware, and that test is gated behind
//! `pure_tests`.

#![allow(dead_code)] // consumed once `run` assembly lands

mod convert;
mod device;
mod errors;
mod frames;
mod pipeline;
mod resample;

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::SyncSender,
};

// The device layer is otherwise an implementation detail of capture, but
// `voice-orders devices` lists exactly what it selects from, and `doctor`
// checks the selection itself.
pub use device::{InputDevice, list_input_devices, select_input_device};

use cpal::traits::{DeviceTrait, StreamTrait};
use tracing_batteries::prelude::*;

use crate::{
    audio::{convert::IntoPcm16, frames::FrameAssembler, pipeline::CapturePipeline},
    errors::HumanizableError,
    recognition::AudioMsg,
};

/// The `audio.device` value which means "whatever the system considers
/// default", as opposed to a substring of a device's name.
pub const DEFAULT_DEVICE_HINT: &str = "default";

/// The length of an audio frame, as a fraction of the target sample rate.
/// Ten frames per second, i.e. ~100 ms each, per DESIGN.md.
const FRAMES_PER_SECOND: u32 = 10;

/// A running audio capture.
///
/// The handle owns the cpal stream, so dropping it stops capture and — once
/// the recognizer's copy of the sender is gone too — closes the audio channel,
/// which is how the recognizer thread learns to shut down.
///
/// cpal's ALSA stream is `Send + Sync` (it owns its own audio thread and the
/// ALSA library is assumed to be thread-safe), so `run` can park this handle
/// wherever it likes on the Tokio side. A Linux-only unit test pins that down.
///
/// **cpal's WASAPI stream is not.** COM apartment state is per-thread, so a
/// Windows `cpal::Stream` is `!Send`, and with it the whole pipeline future
/// which owns this handle: on Windows `Pipeline::run()` must be awaited where
/// it was built (`main`'s thread) and may never be `tokio::spawn`ed. The
/// assembly already awaits it in place, so nothing has to change — but a
/// future refactor which spawns it would compile on Linux and fail only on
/// Windows, which is why this is written down here rather than left to be
/// rediscovered.
#[must_use = "capture stops as soon as the handle is dropped"]
pub struct CaptureHandle {
    stream: cpal::Stream,
    device_name: String,
    input_rate: u32,
    input_channels: u16,
    input_format: cpal::SampleFormat,
    frame_len: usize,
}

impl CaptureHandle {
    /// The name of the device we are recording from.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// The sample rate the device is actually running at, before conversion.
    pub fn input_rate(&self) -> u32 {
        self.input_rate
    }

    /// The number of channels the device is actually delivering, before the
    /// down-mix to mono.
    pub fn input_channels(&self) -> u16 {
        self.input_channels
    }

    /// The sample format the device is actually delivering.
    pub fn input_format(&self) -> cpal::SampleFormat {
        self.input_format
    }

    /// The number of samples in each frame pushed onto the audio channel.
    pub fn frame_len(&self) -> usize {
        self.frame_len
    }

    /// Asks the device to stop delivering audio without tearing the stream
    /// down. Not every backend supports this; muting is done through the
    /// `listening` flag instead, so this is only a power-saving nicety.
    pub fn pause(&self) -> Result<(), crate::Error> {
        self.stream.pause().map_err(|e| e.to_human_error())
    }

    /// Resumes a paused stream.
    pub fn resume(&self) -> Result<(), crate::Error> {
        self.stream.play().map_err(|e| e.to_human_error())
    }

    /// Stops capture without waiting for the handle to be dropped.
    pub fn stop(self) {
        drop(self);
    }
}

impl std::fmt::Debug for CaptureHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureHandle")
            .field("device_name", &self.device_name)
            .field("input_rate", &self.input_rate)
            .field("input_channels", &self.input_channels)
            .field("input_format", &self.input_format)
            .field("frame_len", &self.frame_len)
            .finish_non_exhaustive()
    }
}

/// Opens a microphone and starts feeding ~100 ms frames of 16-bit mono PCM at
/// `target_rate` into `frames`.
///
/// * `device_hint` is `"default"` (or empty) for the system default input
///   device, or a case-insensitive substring of the device's name.
/// * `listening` mirrors the hotkey task's listening state; while it is false
///   the callback discards input at the source, so a muted microphone costs
///   nothing downstream.
/// * `dropped` counts frames which were thrown away because the recognizer
///   could not keep up.
pub fn start_capture(
    device_hint: &str,
    target_rate: u32,
    frames: SyncSender<AudioMsg>,
    listening: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
) -> Result<CaptureHandle, crate::Error> {
    if target_rate == 0 {
        return Err(human_errors::system(
            "We were asked to capture audio at a sample rate of 0 Hz, which is not something any microphone can do.",
            &["Please report this issue on GitHub so that we can investigate."],
        ));
    }

    let host = cpal::default_host();
    let device = device::select_input_device(&host, device_hint)?;
    let device_name = device::device_name(&device);

    let supported = device::choose_input_config(&device, target_rate)?;
    let input_format = supported.sample_format();
    let input_rate = supported.sample_rate().0;
    let input_channels = supported.channels();
    let config = supported.config();

    let frame_len = (target_rate / FRAMES_PER_SECOND).max(1) as usize;

    info!(
        device = device_name.as_str(),
        input_rate,
        input_channels,
        input_format = %input_format,
        target_rate,
        frame_samples = frame_len,
        "Starting audio capture."
    );

    if input_rate != target_rate {
        debug!(
            input_rate,
            target_rate,
            "The device cannot run at the model's rate, so we will resample in the capture callback."
        );
    }

    let pipeline = CapturePipeline::new(
        input_channels,
        input_rate,
        target_rate,
        FrameAssembler::new(frame_len, frames, dropped),
    );

    let stream = match input_format {
        cpal::SampleFormat::I16 => build_stream::<i16>(&device, &config, pipeline, listening),
        cpal::SampleFormat::F32 => build_stream::<f32>(&device, &config, pipeline, listening),
        cpal::SampleFormat::U16 => build_stream::<u16>(&device, &config, pipeline, listening),
        other => Err(human_errors::system(
            format!(
                "We selected an audio format ({other}) which we do not know how to convert to the 16-bit mono audio the speech model needs."
            ),
            &["Please report this issue on GitHub so that we can investigate."],
        )),
    }?;

    stream.play().map_err(|e| e.to_human_error())?;

    Ok(CaptureHandle {
        stream,
        device_name,
        input_rate,
        input_channels,
        input_format,
        frame_len,
    })
}

/// Builds the input stream for a concrete sample type.
fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut pipeline: CapturePipeline,
    listening: Arc<AtomicBool>,
) -> Result<cpal::Stream, crate::Error>
where
    T: cpal::SizedSample + IntoPcm16,
{
    device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                // Realtime thread: no locks, no logging, no unbounded
                // allocation. Everything below is a slice walk plus (at most)
                // one frame-sized allocation per 100 ms.
                if !listening.load(Ordering::Relaxed) {
                    pipeline.discard();
                    return;
                }

                pipeline.push(data);
            },
            |err| warn!("The audio input stream reported a problem: {err}"),
            None,
        )
        .map_err(|e| e.to_human_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    /// `run` assembles the pipeline on the Tokio runtime and needs to be able
    /// to move the handle into a task, so this must keep holding on Linux.
    ///
    /// Linux-only: cpal's WASAPI stream is `!Send` (COM apartments are
    /// per-thread), which is exactly what the type's own documentation
    /// anticipates — asserting it on Windows would be asserting something
    /// false.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_handle_can_be_moved_between_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CaptureHandle>();
    }

    #[test]
    fn a_zero_target_rate_is_rejected_before_any_hardware_is_touched() {
        let (tx, _rx) = sync_channel(8);

        let err = start_capture(
            DEFAULT_DEVICE_HINT,
            0,
            tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicU64::new(0)),
        )
        .expect_err("a zero sample rate cannot be captured");

        assert!(err.description().contains("0 Hz"));
    }

    /// Hardware smoke test: open the default microphone, run for a moment
    /// while "listening", and confirm frames of the right length arrive.
    #[test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    fn the_default_device_produces_frames() {
        use cpal::traits::HostTrait;

        if cpal::default_host().default_input_device().is_none() {
            eprintln!("no default input device on this machine; skipping");
            return;
        }

        let (tx, rx) = sync_channel(8);
        let dropped = Arc::new(AtomicU64::new(0));
        let listening = Arc::new(AtomicBool::new(true));

        let handle = start_capture(
            DEFAULT_DEVICE_HINT,
            16_000,
            tx,
            listening.clone(),
            dropped.clone(),
        )
        .expect("the default input device should be capturable");

        assert_eq!(handle.frame_len(), 1_600);
        assert!(!handle.device_name().is_empty());

        let frame = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a frame should arrive within five seconds");

        match frame {
            AudioMsg::Frame(samples) => assert_eq!(samples.len(), 1_600),
            AudioMsg::Reset | AudioMsg::Clear => {
                panic!("capture must never send a control message")
            }
        }

        handle.stop();
    }
}
