//! `HumanizableError` implementations for cpal's error types.
//!
//! These live here rather than in `src/errors.rs` because the audio module is
//! the only thing in the crate which ever sees a cpal error — see the note at
//! the top of that file.
//!
//! The split between `User` and `System` follows the github-backup convention:
//! anything the person running the tool can fix (an unplugged microphone, a
//! device another application has grabbed exclusively, a profile pointing at a
//! device that cannot do what we need) is a user error; anything that means
//! cpal or the sound server is behaving unexpectedly is a system error.

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

impl HumanizableError for cpal::DevicesError {
    fn to_human_error(self) -> crate::Error {
        human_errors::wrap_system(
            self,
            "We could not ask your system which audio devices are available.",
            BACKEND_ADVICE,
        )
    }
}

impl HumanizableError for cpal::DeviceNameError {
    fn to_human_error(self) -> crate::Error {
        human_errors::wrap_system(
            self,
            "We could not read the name of one of your audio devices.",
            BACKEND_ADVICE,
        )
    }
}

impl HumanizableError for cpal::SupportedStreamConfigsError {
    fn to_human_error(self) -> crate::Error {
        match self {
            cpal::SupportedStreamConfigsError::DeviceNotAvailable => human_errors::wrap_user(
                self,
                "The microphone we selected is no longer available.",
                DEVICE_UNAVAILABLE_ADVICE,
            ),
            _ => human_errors::wrap_system(
                self,
                "We could not work out which audio formats your microphone supports.",
                BACKEND_ADVICE,
            ),
        }
    }
}

impl HumanizableError for cpal::DefaultStreamConfigError {
    fn to_human_error(self) -> crate::Error {
        match self {
            cpal::DefaultStreamConfigError::DeviceNotAvailable => human_errors::wrap_user(
                self,
                "The microphone we selected is no longer available.",
                DEVICE_UNAVAILABLE_ADVICE,
            ),
            cpal::DefaultStreamConfigError::StreamTypeNotSupported => human_errors::wrap_user(
                self,
                "The device we selected does not support recording audio.",
                &[
                    "Set audio.device in your profile to part of the name of a microphone rather than a speaker.",
                    "Set audio.device to 'default' to use your system's default microphone.",
                ],
            ),
            _ => human_errors::wrap_system(
                self,
                "We could not work out the default audio format for your microphone.",
                BACKEND_ADVICE,
            ),
        }
    }
}

impl HumanizableError for cpal::BuildStreamError {
    fn to_human_error(self) -> crate::Error {
        match self {
            cpal::BuildStreamError::DeviceNotAvailable => human_errors::wrap_user(
                self,
                "The microphone we selected disappeared before we could start listening to it.",
                DEVICE_UNAVAILABLE_ADVICE,
            ),
            cpal::BuildStreamError::StreamConfigNotSupported => human_errors::wrap_user(
                self,
                "Your microphone rejected the recording format we asked it for.",
                &[
                    "Set audio.device in your profile to a different microphone.",
                    "If the device is used exclusively by another application, close that application and try again.",
                ],
            ),
            _ => human_errors::wrap_system(
                self,
                "We could not open an audio input stream on your microphone.",
                BACKEND_ADVICE,
            ),
        }
    }
}

impl HumanizableError for cpal::PlayStreamError {
    fn to_human_error(self) -> crate::Error {
        match self {
            cpal::PlayStreamError::DeviceNotAvailable => human_errors::wrap_user(
                self,
                "The microphone we selected disappeared before we could start listening to it.",
                DEVICE_UNAVAILABLE_ADVICE,
            ),
            _ => human_errors::wrap_system(
                self,
                "We could not start recording from your microphone.",
                BACKEND_ADVICE,
            ),
        }
    }
}

impl HumanizableError for cpal::PauseStreamError {
    fn to_human_error(self) -> crate::Error {
        match self {
            cpal::PauseStreamError::DeviceNotAvailable => human_errors::wrap_user(
                self,
                "The microphone we were recording from is no longer available.",
                DEVICE_UNAVAILABLE_ADVICE,
            ),
            _ => human_errors::wrap_system(
                self,
                "We could not pause the recording from your microphone.",
                BACKEND_ADVICE,
            ),
        }
    }
}

impl HumanizableError for cpal::StreamError {
    fn to_human_error(self) -> crate::Error {
        match self {
            cpal::StreamError::DeviceNotAvailable => human_errors::wrap_user(
                self,
                "The microphone we were recording from was disconnected.",
                DEVICE_UNAVAILABLE_ADVICE,
            ),
            _ => human_errors::wrap_system(
                self,
                "The audio input stream failed while we were recording.",
                BACKEND_ADVICE,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use human_errors::Kind;

    fn backend_error() -> cpal::BackendSpecificError {
        cpal::BackendSpecificError {
            description: "the sound server exploded".to_string(),
        }
    }

    #[test]
    fn a_missing_device_is_a_user_error() {
        let err = cpal::BuildStreamError::DeviceNotAvailable.to_human_error();
        assert!(err.is(Kind::User));
        assert!(err.description().contains("microphone"));
        assert!(!err.advice().is_empty());
    }

    #[test]
    fn an_unsupported_config_is_a_user_error() {
        let err = cpal::BuildStreamError::StreamConfigNotSupported.to_human_error();
        assert!(err.is(Kind::User));
        assert!(!err.advice().is_empty());
    }

    #[test]
    fn a_backend_failure_is_a_system_error() {
        let err = cpal::BuildStreamError::BackendSpecific {
            err: backend_error(),
        }
        .to_human_error();
        assert!(err.is(Kind::System));
        assert!(
            err.message().contains("the sound server exploded"),
            "the underlying cpal detail must survive into the rendered message: {}",
            err.message()
        );
    }

    #[test]
    fn enumeration_failures_are_system_errors() {
        let err = cpal::DevicesError::BackendSpecific {
            err: backend_error(),
        }
        .to_human_error();
        assert!(err.is(Kind::System));
    }

    #[test]
    fn a_disconnected_stream_is_a_user_error() {
        let err = cpal::StreamError::DeviceNotAvailable.to_human_error();
        assert!(err.is(Kind::User));
    }

    #[test]
    fn a_playback_device_cannot_be_recorded_from() {
        let err = cpal::DefaultStreamConfigError::StreamTypeNotSupported.to_human_error();
        assert!(err.is(Kind::User));
        assert!(err.description().contains("recording"));
    }

    #[test]
    fn config_enumeration_failures_are_classified_by_variant() {
        assert!(
            cpal::SupportedStreamConfigsError::DeviceNotAvailable
                .to_human_error()
                .is(Kind::User)
        );
        assert!(
            cpal::SupportedStreamConfigsError::InvalidArgument
                .to_human_error()
                .is(Kind::System)
        );
    }

    #[test]
    fn stream_control_failures_are_classified_by_variant() {
        assert!(
            cpal::PlayStreamError::DeviceNotAvailable
                .to_human_error()
                .is(Kind::User)
        );
        assert!(
            cpal::PauseStreamError::DeviceNotAvailable
                .to_human_error()
                .is(Kind::User)
        );
        assert!(
            cpal::DeviceNameError::BackendSpecific {
                err: backend_error(),
            }
            .to_human_error()
            .is(Kind::System)
        );
    }
}
