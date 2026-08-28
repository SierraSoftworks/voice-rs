//! Sample-rate conversion from whatever the device gives us to the model's
//! rate.
//!
//! Two strategies, picked once when the stream is built:
//!
//! * an exact integer ratio (48 kHz → 16 kHz is the common one under PipeWire)
//!   is handled by **averaging decimation** — every group of `factor` input
//!   samples is averaged into one output sample, which is a cheap box filter
//!   and keeps a little of the anti-aliasing that plain "take every third
//!   sample" throws away;
//! * anything else (44.1 kHz → 16 kHz, or upsampling) uses **linear
//!   interpolation** over an exact rational position, so the output is
//!   identical no matter how the input happens to be chopped into callbacks.
//!
//! Both are stateful structs rather than free functions precisely because the
//! cpal callback hands us an arbitrary slice each time and the fractional
//! position (or the partial average) has to survive across those boundaries.
//! Speech recognition is tolerant of naive resampling; if accuracy disappoints,
//! swapping in `rubato` is an isolated change to this file.

/// Resamples a mono 16-bit PCM stream, chunk by chunk.
#[derive(Debug)]
pub enum Resampler {
    /// The device already runs at the model's rate.
    Passthrough,
    /// The input rate is an exact integer multiple of the output rate.
    Decimate(Decimator),
    /// Any other ratio, including upsampling.
    Linear(LinearResampler),
}

impl Resampler {
    /// Chooses a strategy for the given rates.
    pub fn new(input_rate: u32, output_rate: u32) -> Self {
        if input_rate == 0 || output_rate == 0 || input_rate == output_rate {
            return Self::Passthrough;
        }

        if input_rate > output_rate && input_rate.is_multiple_of(output_rate) {
            return Self::Decimate(Decimator::new(input_rate / output_rate));
        }

        Self::Linear(LinearResampler::new(input_rate, output_rate))
    }

    /// Resamples `input`, appending the result to `out`.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        match self {
            Self::Passthrough => out.extend_from_slice(input),
            Self::Decimate(decimator) => decimator.process(input, out),
            Self::Linear(linear) => linear.process(input, out),
        }
    }

    /// Discards any partially accumulated state, as though the stream had just
    /// started. Used when listening is turned off so audio from before the
    /// mute cannot be stitched onto audio from after it.
    pub fn reset(&mut self) {
        match self {
            Self::Passthrough => {}
            Self::Decimate(decimator) => decimator.reset(),
            Self::Linear(linear) => linear.reset(),
        }
    }
}

/// Averaging decimation by an exact integer factor.
#[derive(Debug)]
pub struct Decimator {
    factor: u32,
    accumulator: i32,
    count: u32,
}

impl Decimator {
    /// Creates a decimator which averages every `factor` input samples into
    /// one output sample. A factor of zero is treated as one.
    pub fn new(factor: u32) -> Self {
        Self {
            factor: factor.max(1),
            accumulator: 0,
            count: 0,
        }
    }

    /// Decimates `input`, appending the result to `out`.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        for &sample in input {
            self.accumulator += sample as i32;
            self.count += 1;

            if self.count == self.factor {
                out.push((self.accumulator / self.factor as i32) as i16);
                self.accumulator = 0;
                self.count = 0;
            }
        }
    }

    /// Discards the partially accumulated group.
    pub fn reset(&mut self) {
        self.accumulator = 0;
        self.count = 0;
    }
}

/// Linear interpolation over an exact rational position.
///
/// The position of output sample `k` is `k * input_rate / output_rate` input
/// samples from the start of the stream. That position is tracked as an
/// integer part plus a fraction with denominator `output_rate`, so it never
/// drifts and never depends on how the input was chunked.
#[derive(Debug)]
pub struct LinearResampler {
    output_rate: u32,
    /// Integer part of the per-output-sample step.
    step_whole: u32,
    /// Fractional part of the step, over `output_rate`.
    step_fraction: u32,
    /// Integer part of the next output's source position, relative to the
    /// start of the chunk currently being processed. Always `>= -1`.
    position: i64,
    /// Fractional part of that position, over `output_rate`.
    fraction: u32,
    /// The input sample immediately before the current chunk (source index
    /// `-1`), so an output landing between two chunks interpolates correctly.
    previous: i16,
}

impl LinearResampler {
    /// Creates a resampler converting `input_rate` to `output_rate`.
    pub fn new(input_rate: u32, output_rate: u32) -> Self {
        let output_rate = output_rate.max(1);

        Self {
            output_rate,
            step_whole: input_rate / output_rate,
            step_fraction: input_rate % output_rate,
            position: 0,
            fraction: 0,
            previous: 0,
        }
    }

    /// Resamples `input`, appending the result to `out`.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        let len = input.len() as i64;
        if len == 0 {
            return;
        }

        // An output can be produced while its source position is within the
        // samples we can see: strictly before the last one, or exactly on it
        // when there is no fraction left to interpolate.
        while self.position < len - 1 || (self.position == len - 1 && self.fraction == 0) {
            let before = if self.position < 0 {
                self.previous
            } else {
                input[self.position as usize]
            };

            let sample = if self.fraction == 0 {
                before
            } else {
                // `position + 1` is in range because the loop condition above
                // only allows a non-zero fraction while `position < len - 1`.
                let after = input[(self.position + 1) as usize];
                let t = self.fraction as f32 / self.output_rate as f32;
                (before as f32 + (after as f32 - before as f32) * t).round() as i16
            };

            out.push(sample);
            self.advance();
        }

        self.previous = input[(len - 1) as usize];
        self.position -= len;
    }

    /// Discards the fractional position and the carried-over sample.
    pub fn reset(&mut self) {
        self.position = 0;
        self.fraction = 0;
        self.previous = 0;
    }

    fn advance(&mut self) {
        self.position += self.step_whole as i64;
        self.fraction += self.step_fraction;

        if self.fraction >= self.output_rate {
            self.fraction -= self.output_rate;
            self.position += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn ramp(len: usize) -> Vec<i16> {
        (0..len).map(|i| (i % 1000) as i16).collect()
    }

    fn sine(len: usize, period: f32) -> Vec<i16> {
        (0..len)
            .map(|i| {
                let phase = i as f32 / period * std::f32::consts::TAU;
                (phase.sin() * 12_000.0) as i16
            })
            .collect()
    }

    fn run_in_chunks(resampler: &mut Resampler, input: &[i16], chunk: usize) -> Vec<i16> {
        let mut out = Vec::new();
        for piece in input.chunks(chunk) {
            resampler.process(piece, &mut out);
        }
        out
    }

    #[rstest]
    #[case(16_000, 16_000)]
    #[case(0, 16_000)]
    #[case(16_000, 0)]
    fn identical_or_degenerate_rates_pass_through(#[case] input: u32, #[case] output: u32) {
        assert!(matches!(
            Resampler::new(input, output),
            Resampler::Passthrough
        ));
    }

    #[rstest]
    #[case(48_000, 16_000)]
    #[case(32_000, 16_000)]
    #[case(96_000, 16_000)]
    fn exact_integer_ratios_use_decimation(#[case] input: u32, #[case] output: u32) {
        assert!(matches!(
            Resampler::new(input, output),
            Resampler::Decimate(_)
        ));
    }

    #[rstest]
    #[case(44_100, 16_000)]
    #[case(22_050, 16_000)]
    #[case(8_000, 16_000)]
    fn other_ratios_use_linear_interpolation(#[case] input: u32, #[case] output: u32) {
        assert!(matches!(
            Resampler::new(input, output),
            Resampler::Linear(_)
        ));
    }

    #[test]
    fn passthrough_copies_the_input() {
        let mut resampler = Resampler::new(16_000, 16_000);
        let input = ramp(64);
        assert_eq!(run_in_chunks(&mut resampler, &input, 7), input);
    }

    #[test]
    fn decimation_of_a_ramp_averages_each_group() {
        let mut resampler = Resampler::new(48_000, 16_000);
        let input: Vec<i16> = (0..30i16).collect();
        let out = run_in_chunks(&mut resampler, &input, 30);

        assert_eq!(out.len(), 10);
        // Each output is the mean of three consecutive ramp samples, i.e.
        // (3k) + (3k+1) + (3k+2) / 3 == 3k + 1.
        let expected: Vec<i16> = (0..10i16).map(|k| 3 * k + 1).collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn decimation_produces_a_third_of_the_samples() {
        let mut resampler = Resampler::new(48_000, 16_000);
        let input = sine(48_000, 100.0);
        let out = run_in_chunks(&mut resampler, &input, 1_024);
        assert_eq!(out.len(), 16_000);
    }

    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(7)]
    #[case(480)]
    #[case(1_024)]
    fn decimation_is_independent_of_the_chunking(#[case] chunk: usize) {
        let input = sine(9_600, 73.0);

        let mut reference = Resampler::new(48_000, 16_000);
        let expected = run_in_chunks(&mut reference, &input, input.len());

        let mut resampler = Resampler::new(48_000, 16_000);
        assert_eq!(run_in_chunks(&mut resampler, &input, chunk), expected);
    }

    #[test]
    fn decimation_carries_a_partial_group_across_chunks() {
        // Four samples split 3/1 must still average the *first three*.
        let mut resampler = Resampler::new(48_000, 16_000);
        let mut out = Vec::new();
        resampler.process(&[3, 6], &mut out);
        assert!(out.is_empty());
        resampler.process(&[9, 100], &mut out);
        assert_eq!(out, vec![6]);
    }

    #[test]
    fn resetting_a_decimator_drops_the_partial_group() {
        let mut resampler = Resampler::new(48_000, 16_000);
        let mut out = Vec::new();
        resampler.process(&[3, 6], &mut out);
        resampler.reset();
        resampler.process(&[9, 9, 9], &mut out);
        assert_eq!(out, vec![9]);
    }

    #[test]
    fn linear_resampling_of_one_second_produces_the_target_rate() {
        let mut resampler = Resampler::new(44_100, 16_000);
        let input = sine(44_100, 147.0);
        let out = run_in_chunks(&mut resampler, &input, 441);

        // Output k sits at k * 44100/16000 input samples; the last one which
        // fits inside 44100 samples is k = 15999.
        assert_eq!(out.len(), 16_000);
    }

    #[rstest]
    // The last output of a finite buffer is only produced once the input
    // sample after it has arrived, so a one-second buffer yields one fewer
    // sample than the target rate when the ratio does not land exactly.
    #[case(8_000, 16_000, 8_000, 15_999)]
    #[case(22_050, 16_000, 22_050, 16_000)]
    #[case(44_100, 16_000, 4_410, 1_600)]
    fn linear_resampling_produces_the_expected_number_of_samples(
        #[case] input_rate: u32,
        #[case] output_rate: u32,
        #[case] input_len: usize,
        #[case] expected_len: usize,
    ) {
        let mut resampler = Resampler::new(input_rate, output_rate);
        let input = sine(input_len, 61.0);
        let out = run_in_chunks(&mut resampler, &input, 128);
        assert_eq!(out.len(), expected_len);
    }

    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(147)]
    #[case(441)]
    #[case(1_024)]
    #[case(4_096)]
    fn linear_resampling_is_independent_of_the_chunking(#[case] chunk: usize) {
        let input = sine(44_100, 137.0);

        let mut reference = Resampler::new(44_100, 16_000);
        let expected = run_in_chunks(&mut reference, &input, input.len());

        let mut resampler = Resampler::new(44_100, 16_000);
        let actual = run_in_chunks(&mut resampler, &input, chunk);

        assert_eq!(actual.len(), expected.len());
        assert_eq!(actual, expected);
    }

    #[test]
    fn linear_resampling_interpolates_between_neighbouring_samples() {
        // 3 kHz -> 2 kHz: outputs land at input positions 0, 1.5, 3, 4.5, ...
        let mut resampler = Resampler::new(3_000, 2_000);
        let out = run_in_chunks(&mut resampler, &[0, 100, 200, 300, 400, 600], 6);
        assert_eq!(out, vec![0, 150, 300, 500]);
    }

    #[test]
    fn linear_upsampling_repeats_and_interpolates() {
        // 2 kHz -> 4 kHz: outputs land at input positions 0, 0.5, 1, 1.5, ...
        let mut resampler = Resampler::new(2_000, 4_000);
        let out = run_in_chunks(&mut resampler, &[0, 100, 200], 3);
        assert_eq!(out, vec![0, 50, 100, 150, 200]);
    }

    #[test]
    fn linear_resampling_tracks_a_ramp_monotonically() {
        let mut resampler = Resampler::new(44_100, 16_000);
        let input: Vec<i16> = (0..4_410).map(|i| i as i16 / 8).collect();
        let out = run_in_chunks(&mut resampler, &input, 333);

        assert!(out.windows(2).all(|w| w[1] >= w[0]));
        assert_eq!(out.first().copied(), Some(0));
    }

    #[test]
    fn resetting_a_linear_resampler_restarts_the_position() {
        let mut resampler = Resampler::new(44_100, 16_000);
        let input = sine(2_048, 91.0);

        let mut first = Vec::new();
        resampler.process(&input, &mut first);
        resampler.reset();

        let mut second = Vec::new();
        resampler.process(&input, &mut second);

        assert_eq!(first, second);
    }

    #[test]
    fn empty_chunks_are_ignored() {
        let mut resampler = Resampler::new(44_100, 16_000);
        let mut out = Vec::new();
        resampler.process(&[], &mut out);
        resampler.process(&[500; 64], &mut out);
        resampler.process(&[], &mut out);
        assert!(!out.is_empty());
    }
}
