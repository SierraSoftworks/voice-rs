//! `voice-orders devices [--audio] [--hotkey]`: what this machine has to
//! listen with. See DESIGN.md §"CLI".
//!
//! Both `audio.device` and `hotkey.device` are matched against strings which
//! only the machine knows — a microphone's name, a keyboard's name, an event
//! node's number — and every failure to match them ends the same way: someone
//! guessing at what to type. This command removes the guessing by printing the
//! two lists, in the form the options take, so a value can be copied straight
//! out of it.
//!
//! Two rules shape it:
//!
//! 1. **The sections are independent.** A machine with no read access to
//!    `/dev/input` still has microphones worth listing, and vice versa, so each
//!    section reports its own failure inline and only a run in which *nothing*
//!    could be listed exits non-zero.
//! 2. **The answers are the real ones.** The audio names come from the same
//!    enumeration `audio.device` matches against, and the hotkey ranking is the
//!    one `device: auto` actually uses — including which device it would pick —
//!    rather than a second implementation which could drift away from it.

use clap::Args;
use tracing_batteries::prelude::*;

use crate::audio::{self, InputDevice};
use crate::config::SystemConfig;
#[cfg(target_os = "linux")]
use crate::hotkey::{self, ListedDevice};
#[cfg(target_os = "linux")]
use crate::output::keys;

/// The key the hotkey listing ranks devices for: every keyboard has a left
/// control, and `doctor` probes with the same one for the same reason.
#[cfg(target_os = "linux")]
const PROBE_KEY: &str = "leftctrl";

#[derive(Args, Debug)]
pub struct DevicesArgs {
    /// Only list audio input devices.
    #[arg(long)]
    pub audio: bool,

    /// Only list hotkey (evdev) input devices.
    #[arg(long)]
    pub hotkey: bool,
}

impl DevicesArgs {
    /// Which sections to print. Naming neither flag prints both, which is what
    /// somebody who has just run out of ideas about their profile wants.
    fn sections(&self) -> (bool, bool) {
        match (self.audio, self.hotkey) {
            (false, false) => (true, true),
            (audio, hotkey) => (audio, hotkey),
        }
    }
}

/// Lists this machine's devices, returning the exit code to leave with.
pub async fn run(args: DevicesArgs) -> Result<i32, crate::Error> {
    let (want_audio, want_hotkey) = args.sections();

    // A configuration we cannot read is worth saying out loud — it changes
    // what these listings mean — but it must not stop us listing anything.
    let system = SystemConfig::load().unwrap_or_else(|e| {
        print!("{}", render_error(&e));
        println!();
        SystemConfig::default()
    });

    let mut failures = 0;
    let mut sections = 0;

    if want_audio {
        sections += 1;
        if !print_audio(&system) {
            failures += 1;
        }
    }

    if want_hotkey {
        if sections > 0 {
            println!();
        }
        sections += 1;
        if !print_hotkey() {
            failures += 1;
        }
    }

    let code = i32::from(failures == sections);
    debug!(
        sections,
        failures, "Device listing finished with exit code {code}."
    );

    Ok(code)
}

// ── Audio inputs ────────────────────────────────────────────────────────────

/// Prints the audio section, returning whether it could be listed at all.
fn print_audio(system: &SystemConfig) -> bool {
    match audio::list_input_devices(&cpal::default_host()) {
        Ok(devices) => {
            print!("{}", render_audio(&devices, system.audio.device.as_deref()));
            true
        }
        Err(e) => {
            println!("{AUDIO_HEADING}");
            print!("{}", render_error(&e));
            false
        }
    }
}

const AUDIO_HEADING: &str = "Audio inputs (audio.device)";

/// Renders the audio section from the device list alone.
///
/// `configured` is the machine's `audio.device`, when it has one: a profile
/// which does not name a microphone gets that one, so the listing says which it
/// is — and says so even when nothing matches it, because a stale name in the
/// config file is exactly the kind of thing this command exists to surface.
fn render_audio(devices: &[InputDevice], configured: Option<&str>) -> String {
    let mut out = format!("{AUDIO_HEADING}\n");

    if devices.is_empty() {
        out.push_str("  (none — no audio input devices could be seen)\n");
        return out;
    }

    let selected = configured.and_then(|hint| matching(devices, hint));

    for (index, device) in devices.iter().enumerate() {
        let marker = if device.is_default { '*' } else { ' ' };
        out.push_str(&format!("  {marker} \"{}\"", device.name));

        let mut notes = Vec::new();
        if device.is_default {
            notes.push("system default".to_string());
        }
        if selected == Some(index) {
            notes.push("your audio.device".to_string());
        }
        if !notes.is_empty() {
            out.push_str(&format!(" — {}", notes.join(", ")));
        }

        out.push('\n');
    }

    if let Some(hint) = configured
        && selected.is_none()
    {
        out.push_str(&format!(
            "\n  Your configured audio.device (\"{hint}\") matches none of these.\n"
        ));
    }

    out.push_str(
        "\n  Copy any part of a name into 'audio.device' to use that microphone; matching ignores case.\n",
    );

    out
}

/// Which device an `audio.device` hint selects, resolved the way capture
/// resolves it: `default` means the system default, anything else is a
/// case-insensitive substring, first match wins.
fn matching(devices: &[InputDevice], hint: &str) -> Option<usize> {
    let hint = hint.trim();

    if hint.is_empty() || hint.eq_ignore_ascii_case(audio::DEFAULT_DEVICE_HINT) {
        return devices.iter().position(|device| device.is_default);
    }

    let needle = hint.to_lowercase();
    devices
        .iter()
        .position(|device| device.name.to_lowercase().contains(&needle))
}

// ── Hotkey devices ──────────────────────────────────────────────────────────

/// Prints the hotkey section, returning whether it could be listed at all.
#[cfg(target_os = "linux")]
fn print_hotkey() -> bool {
    let key = keys::from_name(PROBE_KEY).expect("the probe key must be in the key table");

    match hotkey::list_devices(key) {
        Ok(devices) => {
            print!("{}", render_hotkey(&devices));
            true
        }
        Err(e) => {
            println!("{HOTKEY_HEADING}");
            print!("{}", render_error(&e));
            false
        }
    }
}

/// Prints the hotkey section on Windows, where there is nothing to list.
///
/// `hotkey.device` picks between `/dev/input/event*` nodes, which is a Linux
/// concept: the Windows hotkey is a system-wide low-level keyboard hook with no
/// per-device view (DESIGN.md §"Windows support"). Saying so is a successful
/// listing — the section answered the question — so it does not count towards
/// the exit code.
#[cfg(not(target_os = "linux"))]
fn print_hotkey() -> bool {
    println!("{HOTKEY_HEADING}");
    println!(
        "  Device selection is not available on Windows: the listen hotkey is watched system-wide rather than on one keyboard, so 'hotkey.device' has no effect here. Leave it out of your profile."
    );
    true
}

const HOTKEY_HEADING: &str = "Hotkey devices (hotkey.device)";

/// Renders the hotkey section from the ranked device list alone.
#[cfg(target_os = "linux")]
fn render_hotkey(devices: &[ListedDevice]) -> String {
    let mut out = format!("{HOTKEY_HEADING}\n");

    if devices.is_empty() {
        out.push_str("  (none — no input devices could be read)\n");
        return out;
    }

    let chosen = hotkey::auto_choice(devices);
    let width = devices
        .iter()
        .map(|device| device.path.display().to_string().len())
        .max()
        .unwrap_or(0);

    for (index, device) in devices.iter().enumerate() {
        let marker = if chosen == Some(index) { '*' } else { ' ' };
        let path = device.path.display().to_string();
        out.push_str(&format!(
            "  {marker} {path:<width$}  \"{}\" — {}",
            device.name,
            device.rank.describe()
        ));

        if chosen == Some(index) {
            out.push_str("; 'device: auto' picks this one");
        }

        out.push('\n');
    }

    if chosen.is_none() {
        out.push_str(&format!(
            "\n  Nothing here reports '{PROBE_KEY}', so 'device: auto' has nothing to prefer; the ranking above is what it would choose between for another key.\n"
        ));
    }

    out.push_str(
        "\n  Copy a path, or any part of a name, into 'hotkey.device'; 'auto' picks the best-ranked device which reports your key.\n",
    );

    out
}

// ── Failures ────────────────────────────────────────────────────────────────

/// A section which could not be listed, in `doctor`'s shape: the humanized
/// message, then its advice indented beneath it.
fn render_error(error: &crate::Error) -> String {
    let mut out = format!("  ✗ {}\n", error.description());

    for tip in error.advice() {
        out.push_str(&format!("    {tip}\n"));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    #[cfg(target_os = "linux")]
    use std::path::PathBuf;

    fn input(name: &str, is_default: bool) -> InputDevice {
        InputDevice {
            name: name.to_string(),
            is_default,
        }
    }

    #[cfg(target_os = "linux")]
    fn listed(event: u32, name: &str, rank: hotkey::Rank, reports_key: bool) -> ListedDevice {
        ListedDevice {
            path: PathBuf::from(format!("/dev/input/event{event}")),
            name: name.to_string(),
            rank,
            reports_key,
        }
    }

    fn microphones() -> Vec<InputDevice> {
        vec![
            input("HD Audio Analog", true),
            input("Yeti Stereo Microphone", false),
        ]
    }

    // --- Sections ---------------------------------------------------------

    #[rstest]
    #[case::neither(false, false, (true, true))]
    #[case::audio_only(true, false, (true, false))]
    #[case::hotkey_only(false, true, (false, true))]
    #[case::both(true, true, (true, true))]
    fn test_the_flags_choose_the_sections(
        #[case] audio: bool,
        #[case] hotkey: bool,
        #[case] expected: (bool, bool),
    ) {
        assert_eq!(DevicesArgs { audio, hotkey }.sections(), expected);
    }

    // --- Audio ------------------------------------------------------------

    #[test]
    fn test_the_audio_listing_marks_the_system_default() {
        let rendered = render_audio(&microphones(), None);

        assert!(
            rendered.contains("  * \"HD Audio Analog\" — system default\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("    \"Yeti Stereo Microphone\"\n"),
            "a device with nothing to say about it gets no annotation: {rendered}"
        );
        assert!(
            rendered.contains("'audio.device'"),
            "the listing should say what to do with the names: {rendered}"
        );
    }

    #[test]
    fn test_the_audio_listing_marks_the_configured_device() {
        let rendered = render_audio(&microphones(), Some("yeti"));

        assert!(
            rendered.contains("\"Yeti Stereo Microphone\" — your audio.device"),
            "a substring of the name should select it, ignoring case: {rendered}"
        );
        assert!(
            rendered.contains("* \"HD Audio Analog\" — system default\n"),
            "the system default is still the system default: {rendered}"
        );
    }

    #[test]
    fn test_a_configured_default_lands_on_the_default_device() {
        let rendered = render_audio(&microphones(), Some("default"));

        assert!(
            rendered.contains("\"HD Audio Analog\" — system default, your audio.device"),
            "{rendered}"
        );
    }

    #[test]
    fn test_a_configured_device_which_matches_nothing_is_called_out() {
        let rendered = render_audio(&microphones(), Some("Røde"));

        assert!(
            rendered.contains("matches none of these"),
            "a stale config value is exactly what this command is for: {rendered}"
        );
        assert!(rendered.contains("Røde"), "{rendered}");
    }

    #[test]
    fn test_an_empty_audio_listing_says_so() {
        let rendered = render_audio(&[], Some("Yeti"));

        assert!(rendered.contains("(none"), "{rendered}");
    }

    // --- Hotkey -----------------------------------------------------------

    #[cfg(target_os = "linux")]
    #[test]
    fn test_the_hotkey_listing_marks_what_auto_would_pick() {
        let rendered = render_hotkey(&[
            listed(0, "Power Button", hotkey::Rank::ReportsTheKey, false),
            listed(3, "Yubico YubiKey", hotkey::Rank::Types, true),
            listed(
                5,
                "AT Translated Set 2 keyboard",
                hotkey::Rank::Keyboard,
                true,
            ),
        ]);

        assert!(
            rendered.contains(
                "* /dev/input/event5  \"AT Translated Set 2 keyboard\" — keyboard; 'device: auto' picks this one"
            ),
            "the best-ranked candidate wins, wherever it enumerates: {rendered}"
        );
        assert!(
            rendered.contains(
                "    /dev/input/event3  \"Yubico YubiKey\" — types (boot-keyboard set only)\n"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("\"Power Button\" — not a keyboard\n"),
            "{rendered}"
        );
        assert_eq!(
            rendered.matches('*').count(),
            1,
            "exactly one device is the one auto picks: {rendered}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_a_device_which_does_not_report_the_key_is_never_picked() {
        // The listing shows every readable device, but `auto` only ever
        // considers the ones which report the key — even a perfect keyboard.
        let rendered = render_hotkey(&[
            listed(0, "Silent Keyboard", hotkey::Rank::Keyboard, false),
            listed(1, "Odd Remote", hotkey::Rank::ReportsTheKey, true),
        ]);

        assert!(
            rendered.contains("* /dev/input/event1  \"Odd Remote\""),
            "{rendered}"
        );
        assert!(!rendered.contains("* /dev/input/event0"), "{rendered}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_nothing_reporting_the_key_is_explained() {
        let rendered = render_hotkey(&[listed(
            0,
            "Power Button",
            hotkey::Rank::ReportsTheKey,
            false,
        )]);

        assert!(rendered.contains("has nothing to prefer"), "{rendered}");
        assert!(!rendered.contains('*'), "nothing was picked: {rendered}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_an_empty_hotkey_listing_says_so() {
        assert!(render_hotkey(&[]).contains("(none"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_the_paths_line_up() {
        let rendered = render_hotkey(&[
            listed(3, "Short", hotkey::Rank::Keyboard, true),
            listed(10, "Long", hotkey::Rank::Types, true),
        ]);

        let columns: Vec<usize> = rendered
            .lines()
            .filter(|line| line.contains("/dev/input/"))
            .map(|line| line.find('"').expect("every device line quotes a name"))
            .collect();

        assert_eq!(
            columns.len(),
            2,
            "both devices should have been listed: {rendered}"
        );
        assert_eq!(columns[0], columns[1], "names should line up: {rendered}");
    }

    // --- Failures ---------------------------------------------------------

    #[test]
    fn test_a_failed_section_carries_its_advice() {
        let rendered = render_error(&human_errors::user(
            "We were not allowed to read /dev/input/event3.",
            &["Add yourself to the 'input' group."],
        ));

        assert_eq!(
            rendered,
            "  ✗ We were not allowed to read /dev/input/event3.\n    Add yourself to the 'input' group.\n"
        );
    }

    /// Real hardware: both listings must produce something we can render.
    #[test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    fn real_devices_can_be_listed() {
        let devices = audio::list_input_devices(&cpal::default_host())
            .expect("the host should enumerate its inputs");
        let rendered = render_audio(&devices, None);
        assert!(rendered.starts_with(AUDIO_HEADING), "{rendered}");

        #[cfg(target_os = "linux")]
        {
            let key = keys::from_name(PROBE_KEY).expect("the probe key");
            match hotkey::list_devices(key) {
                Ok(devices) => {
                    let rendered = render_hotkey(&devices);
                    assert!(rendered.starts_with(HOTKEY_HEADING), "{rendered}");
                }
                // A machine where /dev/input is unreadable is a legitimate
                // outcome; what matters is that it fails humanly.
                Err(e) => assert!(e.is(human_errors::Kind::User), "{}", e.message()),
            }
        }
    }
}
