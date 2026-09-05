//! `HumanizableError` implementation for cpal's error type.
//!
//! This lives here rather than in `src/errors.rs` because the audio module is
//! the only thing in the crate which ever sees a cpal error — see the note at
//! the top of that file.
//!
//! cpal reports every failure through a single [`cpal::Error`] whose
//! [`kind()`](cpal::Error::kind) says what went wrong, so classification is
//! by kind rather than by which operation was attempted. The split between
//! `User` and `System` follows the github-backup convention: anything the
//! person running the tool can fix (an unplugged microphone, a device another
//! application has grabbed exclusively, a profile pointing at a device that
//! cannot do what we need) is a user error; anything that means cpal or the
//! sound server is behaving unexpectedly is a system error.

use cpal::ErrorKind;

use crate::errors::HumanizableError;

/// Advice shared by every "the device vanished" failure.
const DEVICE_UNAVAILABLE_ADVICE: &[&str] = &[
    "Make sure your microphone is still plugged in and enabled in your sound settings.",
    "Set audio.device in your profile to 'default', or to part of the name of a microphone which is always present.",
];

/// Advice shared by every "this looks like a bug or a broken sound stack" failure.
const BACKEND_ADVICE: &[&str] = &[
    "Make sure PipeWire or PulseAudio is running and that the ALSA compatibility layer is installed.",
    "Please report this issue on GitHub if your microphone works in other applications.",
];

impl HumanizableError for cpal::Error {
    fn to_human_error(self) -> crate::Error {
        match self.kind() {
            ErrorKind::DeviceNotAvailable | ErrorKind::StreamInvalidated => {
                human_errors::wrap_user(
                    self,
                    "The microphone we selected is no longer available.",
                    DEVICE_UNAVAILABLE_ADVICE,
                )
            }
            ErrorKind::DeviceBusy => human_errors::wrap_user(
                self,
                "Another application is using the microphone we selected.",
                &[
                    "Close the application which is using the microphone and try again.",
                    "Set audio.device in your profile to a different microphone.",
                ],
            ),
            ErrorKind::PermissionDenied => human_errors::wrap_user(
                self,
                "Your system refused to let us use the microphone we selected.",
                &[
                    "Make sure your user is allowed to use audio devices (on Linux this usually means membership of the 'audio' group).",
                    "Check your desktop's privacy settings to make sure microphone access is allowed.",
                ],
            ),
            ErrorKind::UnsupportedConfig => human_errors::wrap_user(
                self,
                "The device we selected cannot record audio in the format we asked it for.",
                &[
                    "Set audio.device in your profile to part of the name of a microphone rather than a speaker.",
                    "Set audio.device to 'default' to use your system's default microphone.",
                    "If the device is used exclusively by another application, close that application and try again.",
                ],
            ),
            ErrorKind::HostUnavailable => human_errors::wrap_user(
                self,
                "We could not reach your system's sound server.",
                BACKEND_ADVICE,
            ),
            ErrorKind::Xrun => human_errors::wrap_user(
                self,
                "Your system could not keep up with the audio coming from your microphone, so some of it was lost.",
                &[
                    "Close other applications which are using a lot of CPU and try again.",
                    "If this happens often, ask your sound server for a larger buffer (on PipeWire, raise its quantum).",
                ],
            ),
            _ => human_errors::wrap_system(
                self,
                "Your microphone's audio backend reported an unexpected error.",
                BACKEND_ADVICE,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use human_errors::Kind;
    use rstest::rstest;

    #[rstest]
    #[case::device_not_available(ErrorKind::DeviceNotAvailable, Kind::User)]
    #[case::stream_invalidated(ErrorKind::StreamInvalidated, Kind::User)]
    #[case::device_busy(ErrorKind::DeviceBusy, Kind::User)]
    #[case::permission_denied(ErrorKind::PermissionDenied, Kind::User)]
    #[case::unsupported_config(ErrorKind::UnsupportedConfig, Kind::User)]
    #[case::host_unavailable(ErrorKind::HostUnavailable, Kind::User)]
    #[case::xrun(ErrorKind::Xrun, Kind::User)]
    #[case::backend_error(ErrorKind::BackendError, Kind::System)]
    #[case::other(ErrorKind::Other, Kind::System)]
    #[case::invalid_input(ErrorKind::InvalidInput, Kind::System)]
    fn errors_are_classified_by_kind(#[case] kind: ErrorKind, #[case] expected: Kind) {
        let err = cpal::Error::new(kind).to_human_error();
        let message = format!("{kind:?} should be a {expected:?} error");
        assert!(err.is(expected), "{message}");
        assert!(!err.advice().is_empty());
    }

    #[test]
    fn a_missing_device_talks_about_the_microphone() {
        let err = cpal::Error::new(ErrorKind::DeviceNotAvailable).to_human_error();
        assert!(err.description().contains("microphone"));
    }

    #[test]
    fn the_backend_detail_survives_into_the_rendered_message() {
        let err = cpal::Error::with_message(ErrorKind::BackendError, "the sound server exploded")
            .to_human_error();
        assert!(
            err.message().contains("the sound server exploded"),
            "the underlying cpal detail must survive into the rendered message: {}",
            err.message()
        );
    }
}
