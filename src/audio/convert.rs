//! Sample-format conversion and channel down-mixing.
//!
//! Everything here is a pure function over slices so that the awkward half of
//! the capture path (the half that actually has to be correct) can be tested
//! without a sound card. See DESIGN.md §"Audio capture (cpal)".

/// A sample format cpal may hand us which we know how to turn into the 16-bit
/// signed PCM that Vosk expects.
///
/// The bound deliberately includes `Send + 'static` because every implementor
/// ends up captured by the realtime stream callback.
pub trait IntoPcm16: Copy + Send + 'static {
    /// Converts this sample into signed 16-bit PCM.
    fn into_pcm16(self) -> i16;
}

impl IntoPcm16 for i16 {
    #[inline]
    fn into_pcm16(self) -> i16 {
        self
    }
}

impl IntoPcm16 for u16 {
    /// Unsigned 16-bit PCM has its origin at `1 << 15`, so the conversion is a
    /// re-centring rather than a rescale.
    #[inline]
    fn into_pcm16(self) -> i16 {
        (self as i32 - 32_768) as i16
    }
}

impl IntoPcm16 for f32 {
    /// Floating point PCM is nominally `-1.0..=1.0`; anything outside that is
    /// clamped rather than allowed to wrap into a loud click.
    #[inline]
    fn into_pcm16(self) -> i16 {
        if self.is_nan() {
            return 0;
        }

        (self.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
    }
}

/// Converts an interleaved multi-channel buffer into mono 16-bit PCM by
/// averaging each frame's channels, appending the result to `out`.
///
/// A trailing partial frame (which cpal never produces) is ignored.
pub fn downmix_to_mono<T: IntoPcm16>(interleaved: &[T], channels: u16, out: &mut Vec<i16>) {
    let channels = channels.max(1) as usize;

    if channels == 1 {
        out.extend(interleaved.iter().map(|s| s.into_pcm16()));
        return;
    }

    for frame in interleaved.chunks_exact(channels) {
        let sum: i32 = frame.iter().map(|s| s.into_pcm16() as i32).sum();
        out.push((sum / channels as i32) as i16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case(0i16, 0)]
    #[case(1234i16, 1234)]
    #[case(i16::MAX, i16::MAX)]
    #[case(i16::MIN, i16::MIN)]
    fn i16_conversion_is_the_identity(#[case] input: i16, #[case] expected: i16) {
        assert_eq!(input.into_pcm16(), expected);
    }

    #[rstest]
    #[case(0u16, i16::MIN)]
    #[case(32_768u16, 0)]
    #[case(u16::MAX, i16::MAX)]
    #[case(32_769u16, 1)]
    #[case(16_384u16, -16_384)]
    fn u16_conversion_recentres_the_origin(#[case] input: u16, #[case] expected: i16) {
        assert_eq!(input.into_pcm16(), expected);
    }

    #[rstest]
    #[case(0.0f32, 0)]
    #[case(1.0f32, i16::MAX)]
    #[case(-1.0f32, -i16::MAX)]
    #[case(0.5f32, 16_384)]
    #[case(-0.5f32, -16_384)]
    fn f32_conversion_scales_to_full_range(#[case] input: f32, #[case] expected: i16) {
        assert_eq!(input.into_pcm16(), expected);
    }

    #[rstest]
    #[case(4.0f32, i16::MAX)]
    #[case(-4.0f32, -i16::MAX)]
    #[case(f32::INFINITY, i16::MAX)]
    #[case(f32::NEG_INFINITY, -i16::MAX)]
    #[case(f32::NAN, 0)]
    fn f32_conversion_clamps_instead_of_wrapping(#[case] input: f32, #[case] expected: i16) {
        assert_eq!(input.into_pcm16(), expected);
    }

    #[test]
    fn mono_input_passes_through_unchanged() {
        let mut out = Vec::new();
        downmix_to_mono(&[1i16, -2, 3, -4], 1, &mut out);
        assert_eq!(out, vec![1, -2, 3, -4]);
    }

    #[test]
    fn stereo_input_is_averaged_per_frame() {
        let mut out = Vec::new();
        downmix_to_mono(&[100i16, 200, -100, 100, 0, 0], 2, &mut out);
        assert_eq!(out, vec![150, 0, 0]);
    }

    #[test]
    fn four_channel_input_is_averaged_per_frame() {
        let mut out = Vec::new();
        downmix_to_mono(&[4i16, 8, 12, 16, 0, 0, 0, 4], 4, &mut out);
        assert_eq!(out, vec![10, 1]);
    }

    #[test]
    fn stereo_f32_input_is_converted_then_averaged() {
        let mut out = Vec::new();
        downmix_to_mono(&[1.0f32, -1.0, 0.5, 0.5], 2, &mut out);
        assert_eq!(out, vec![0, 16_384]);
    }

    #[test]
    fn a_trailing_partial_frame_is_ignored() {
        let mut out = Vec::new();
        downmix_to_mono(&[10i16, 20, 30], 2, &mut out);
        assert_eq!(out, vec![15]);
    }

    #[test]
    fn zero_channels_is_treated_as_mono() {
        let mut out = Vec::new();
        downmix_to_mono(&[7i16, 9], 0, &mut out);
        assert_eq!(out, vec![7, 9]);
    }

    #[test]
    fn downmixing_appends_to_the_existing_buffer() {
        let mut out = vec![-1i16];
        downmix_to_mono(&[2i16, 4], 2, &mut out);
        assert_eq!(out, vec![-1, 3]);
    }
}
