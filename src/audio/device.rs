//! Picking a microphone and an input format.
//!
//! Device selection follows the profile's `audio.device` option: `default`
//! uses the host's default input device, anything else is a case-insensitive
//! substring of the device name. When nothing matches, the error *message*
//! lists every input device we could see, because the advice array has to be
//! `&'static` and a list of your microphones is anything but.
//!
//! Format selection prefers exactly what the model wants (the target rate,
//! mono, `i16`) and otherwise takes the nearest configuration we know how to
//! convert from — see `convert` and `resample` for the conversion itself.

use cpal::traits::{DeviceTrait, HostTrait};

use crate::{audio::DEFAULT_DEVICE_HINT, errors::HumanizableError};

/// The sample formats we know how to turn into 16-bit mono PCM, best first.
/// `i16` needs no conversion at all, `f32` is what PipeWire usually offers,
/// and `u16` is a re-centring away.
pub const SUPPORTED_FORMATS: [cpal::SampleFormat; 3] = [
    cpal::SampleFormat::I16,
    cpal::SampleFormat::F32,
    cpal::SampleFormat::U16,
];

/// Finds the input device named by `hint`.
pub fn select_input_device(host: &cpal::Host, hint: &str) -> Result<cpal::Device, crate::Error> {
    let hint = hint.trim();

    if hint.is_empty() || hint.eq_ignore_ascii_case(DEFAULT_DEVICE_HINT) {
        return host.default_input_device().ok_or_else(|| {
            human_errors::user(
                "We could not find a default audio input device to listen on.",
                &[
                    "Make sure that a microphone is plugged in and enabled in your sound settings.",
                    "Set audio.device in your profile to part of the name of the microphone you want to use.",
                ],
            )
        });
    }

    let needle = hint.to_lowercase();
    let mut seen = Vec::new();

    for device in host.input_devices().map_err(|e| e.to_human_error())? {
        let name = device_name(&device);

        if name.to_lowercase().contains(&needle) {
            return Ok(device);
        }

        seen.push(name);
    }

    Err(human_errors::user(
        no_match_message(hint, &seen),
        &[
            "Set audio.device to 'default' to use your system's default microphone.",
            "Set audio.device to part of one of the device names we listed (matching ignores case).",
        ],
    ))
}

/// One input device, as `voice-orders devices` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDevice {
    /// The device's name — the string `audio.device` substring-matches.
    pub name: String,
    /// Whether this is the device `audio.device: default` would pick.
    pub is_default: bool,
}

/// Every input device the host can see, in enumeration order, with the system
/// default marked.
///
/// This is the listing half of [`select_input_device`]: the same host, the same
/// names, so what `devices` prints is exactly what `audio.device` matches
/// against. A default device which does not appear in the enumeration (some
/// backends keep it separate) is added at the front rather than left out, since
/// it is the one every profile gets without asking.
pub fn list_input_devices(host: &cpal::Host) -> Result<Vec<InputDevice>, crate::Error> {
    let default = host
        .default_input_device()
        .map(|device| device_name(&device));

    let mut devices = Vec::new();
    let mut seen_default = false;

    for device in host.input_devices().map_err(|e| e.to_human_error())? {
        let name = device_name(&device);
        let is_default = !seen_default && Some(&name) == default.as_ref();
        seen_default |= is_default;

        devices.push(InputDevice { name, is_default });
    }

    if let Some(name) = default
        && !seen_default
    {
        devices.insert(
            0,
            InputDevice {
                name,
                is_default: true,
            },
        );
    }

    Ok(devices)
}

/// The device's name, or a placeholder when the backend refuses to tell us.
/// Only ever used for matching and for messages, so a failure here is not
/// worth aborting capture over.
pub fn device_name(device: &cpal::Device) -> String {
    device
        .name()
        .unwrap_or_else(|_| "<unnamed device>".to_string())
}

fn no_match_message(hint: &str, seen: &[String]) -> String {
    if seen.is_empty() {
        return format!(
            "We could not find an audio input device whose name contains \"{hint}\" — in fact we could not see any audio input devices at all."
        );
    }

    format!(
        "We could not find an audio input device whose name contains \"{hint}\". The input devices we can see are: {}.",
        seen.join(", ")
    )
}

/// Chooses an input configuration for `device`, preferring `target_rate` mono
/// `i16` and otherwise taking the nearest configuration we can convert from.
pub fn choose_input_config(
    device: &cpal::Device,
    target_rate: u32,
) -> Result<cpal::SupportedStreamConfig, crate::Error> {
    let ranges: Vec<cpal::SupportedStreamConfigRange> = match device.supported_input_configs() {
        Ok(ranges) => ranges.collect(),
        // Some backends refuse to enumerate; the default config is still worth
        // a try before we give up on the device entirely.
        Err(_) => Vec::new(),
    };

    if let Some(exact) = ranges
        .iter()
        .find(|range| is_exact_match(range, target_rate))
    {
        return Ok(exact.with_sample_rate(cpal::SampleRate(target_rate)));
    }

    let nearest = ranges
        .iter()
        .filter(|range| format_rank(range.sample_format()).is_some())
        .min_by_key(|range| rank(range, target_rate));

    if let Some(range) = nearest {
        let rate = cpal::SampleRate(achievable_rate(range, target_rate));

        if let Some(config) = range.try_with_sample_rate(rate) {
            return Ok(config);
        }
    }

    let default = device
        .default_input_config()
        .map_err(|e| e.to_human_error())?;

    if format_rank(default.sample_format()).is_some() {
        return Ok(default);
    }

    Err(human_errors::user(
        format!(
            "Your microphone only offers audio in formats we cannot read (its default is {}, and we understand {}).",
            default.sample_format(),
            SUPPORTED_FORMATS
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        &[
            "Set audio.device in your profile to a different microphone.",
            "If you are using PipeWire or PulseAudio, make sure the ALSA compatibility layer is installed so that the device is offered in a common format.",
        ],
    ))
}

/// Whether this range is exactly the model's preferred format: mono, `i16`,
/// and able to run at the target rate.
fn is_exact_match(range: &cpal::SupportedStreamConfigRange, target_rate: u32) -> bool {
    range.channels() == 1
        && range.sample_format() == cpal::SampleFormat::I16
        && range.min_sample_rate().0 <= target_rate
        && range.max_sample_rate().0 >= target_rate
}

/// The rate this range can run at which is closest to the target.
fn achievable_rate(range: &cpal::SupportedStreamConfigRange, target_rate: u32) -> u32 {
    let min = range.min_sample_rate().0;
    let max = range.max_sample_rate().0;

    if target_rate < min {
        min
    } else if target_rate > max {
        max
    } else {
        target_rate
    }
}

/// Ranks a configuration range; lower sorts better.
///
/// The target rate itself wins, then exact integer multiples of it (they
/// decimate cleanly), then whatever is numerically closest. Ties are broken
/// towards fewer channels and then towards the cheapest sample format.
fn rank(range: &cpal::SupportedStreamConfigRange, target_rate: u32) -> (u8, u64, u16, u8) {
    let rate = achievable_rate(range, target_rate);

    let rate_class = if rate == target_rate {
        0
    } else if target_rate > 0 && rate.is_multiple_of(target_rate) {
        1
    } else {
        2
    };

    (
        rate_class,
        rate.abs_diff(target_rate) as u64,
        range.channels(),
        format_rank(range.sample_format()).unwrap_or(u8::MAX),
    )
}

/// How much we like a sample format, or `None` if we cannot convert it.
fn format_rank(format: cpal::SampleFormat) -> Option<u8> {
    SUPPORTED_FORMATS
        .iter()
        .position(|candidate| *candidate == format)
        .map(|position| position as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::{SampleFormat, SampleRate, SupportedBufferSize, SupportedStreamConfigRange};
    use rstest::rstest;

    fn range(
        channels: u16,
        min: u32,
        max: u32,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(
            channels,
            SampleRate(min),
            SampleRate(max),
            SupportedBufferSize::Unknown,
            format,
        )
    }

    #[test]
    fn the_model_format_is_recognised_as_an_exact_match() {
        assert!(is_exact_match(
            &range(1, 8_000, 48_000, SampleFormat::I16),
            16_000
        ));
    }

    #[rstest]
    #[case::stereo(range(2, 8_000, 48_000, SampleFormat::I16))]
    #[case::float(range(1, 8_000, 48_000, SampleFormat::F32))]
    #[case::wrong_rate(range(1, 44_100, 44_100, SampleFormat::I16))]
    fn anything_else_is_not_an_exact_match(#[case] candidate: SupportedStreamConfigRange) {
        assert!(!is_exact_match(&candidate, 16_000));
    }

    #[rstest]
    #[case(8_000, 48_000, 16_000)]
    #[case(44_100, 48_000, 44_100)]
    #[case(8_000, 11_025, 11_025)]
    #[case(16_000, 16_000, 16_000)]
    fn the_achievable_rate_is_clamped_into_the_supported_range(
        #[case] min: u32,
        #[case] max: u32,
        #[case] expected: u32,
    ) {
        let candidate = range(1, min, max, SampleFormat::I16);
        assert_eq!(achievable_rate(&candidate, 16_000), expected);
    }

    #[test]
    fn the_target_rate_outranks_a_multiple_of_it() {
        let exact = rank(&range(2, 16_000, 16_000, SampleFormat::F32), 16_000);
        let multiple = rank(&range(2, 48_000, 48_000, SampleFormat::F32), 16_000);
        assert!(exact < multiple);
    }

    #[test]
    fn a_multiple_of_the_target_rate_outranks_an_awkward_ratio() {
        let multiple = rank(&range(2, 48_000, 48_000, SampleFormat::F32), 16_000);
        let awkward = rank(&range(2, 44_100, 44_100, SampleFormat::F32), 16_000);
        assert!(multiple < awkward);
    }

    #[test]
    fn fewer_channels_break_a_rate_tie() {
        let mono = rank(&range(1, 48_000, 48_000, SampleFormat::F32), 16_000);
        let stereo = rank(&range(2, 48_000, 48_000, SampleFormat::F32), 16_000);
        assert!(mono < stereo);
    }

    #[test]
    fn i16_breaks_a_channel_tie() {
        let integer = rank(&range(2, 48_000, 48_000, SampleFormat::I16), 16_000);
        let float = rank(&range(2, 48_000, 48_000, SampleFormat::F32), 16_000);
        assert!(integer < float);
    }

    #[rstest]
    #[case(SampleFormat::I16, true)]
    #[case(SampleFormat::F32, true)]
    #[case(SampleFormat::U16, true)]
    #[case(SampleFormat::I8, false)]
    #[case(SampleFormat::I32, false)]
    #[case(SampleFormat::F64, false)]
    fn only_the_three_convertible_formats_are_ranked(
        #[case] format: SampleFormat,
        #[case] supported: bool,
    ) {
        assert_eq!(format_rank(format).is_some(), supported);
    }

    #[test]
    fn a_no_match_message_lists_the_devices_we_can_see() {
        let message = no_match_message(
            "usb mic",
            &[
                "HD Audio Analog".to_string(),
                "Yeti Stereo Microphone".to_string(),
            ],
        );

        assert!(message.contains("usb mic"));
        assert!(message.contains("HD Audio Analog"));
        assert!(message.contains("Yeti Stereo Microphone"));
    }

    #[test]
    fn a_no_match_message_says_so_when_there_are_no_devices() {
        let message = no_match_message("usb mic", &[]);

        assert!(message.contains("usb mic"));
        assert!(message.contains("could not see any audio input devices"));
    }

    /// Hardware smoke test: enumerating devices must not blow up, and the
    /// default device (if any) must offer a configuration we can convert.
    #[test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    fn the_host_can_be_enumerated() {
        let host = cpal::default_host();

        let devices = host
            .input_devices()
            .expect("the host should be able to enumerate its input devices");

        for device in devices {
            let name = device_name(&device);
            assert!(!name.is_empty(), "an input device reported an empty name");
        }

        if let Some(device) = host.default_input_device() {
            let config = choose_input_config(&device, 16_000)
                .expect("the default input device should offer a convertible configuration");

            assert!(format_rank(config.sample_format()).is_some());
            assert!(config.channels() >= 1);
            assert!(config.sample_rate().0 > 0);
        }
    }
}
