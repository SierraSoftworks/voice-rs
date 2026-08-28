//! Finding the evdev device which carries the listen hotkey.
//!
//! The `hotkey.device` profile field (DESIGN.md §"Profile schema") accepts
//! three forms, all resolved here:
//!
//! - `auto` — the first device which reports the configured key;
//! - `/dev/input/eventN` — that exact device node;
//! - anything else — the first device whose name contains the hint
//!   (case-insensitively) *and* reports the configured key.
//!
//! "First" is subtler than it looks. Plenty of things which are not keyboards
//! present themselves as one: a YubiKey types its one-time passwords through a
//! standard HID boot keyboard, so it reports the whole alphabet, the space bar
//! *and* both control keys — which was enough to make it the device `auto`
//! settled on, ahead of the keyboard the person was actually typing at.
//!
//! So candidates which report the key are ranked (see [`rank`]) rather than
//! taken in enumeration order, and the fallback at the bottom of the ranking is
//! the historical behaviour: the first device which reports the key at all.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use tracing_batteries::prelude::*;

use crate::errors::HumanizableError;
use crate::output::KeyCode;

/// Where the kernel exposes evdev device nodes.
const INPUT_DIR: &str = "/dev/input";

/// The literal `hotkey.device` value which asks us to pick a device ourselves.
const AUTO: &str = "auto";

/// The first evdev code outside the standard HID boot-keyboard set.
///
/// This is the second half of telling a keyboard from something which merely
/// types. A device which injects keystrokes — a security key, a barcode
/// scanner, a macro pad in boot-keyboard mode — advertises the boot set and
/// stops there, all of which falls below this code. A keyboard the kernel
/// drives as a keyboard advertises the extended set as well: the media keys,
/// `KEY_F13`..`KEY_F24`, and the vendor extras above them.
const EXTENDED_KEY_FLOOR: u16 = 128;

/// The keys we expect every device a person actually types on to report: the
/// twenty-six letters and the space bar.
///
/// This is deliberately a *typing* set rather than a modifier set. Devices
/// which emit keystrokes without being keyboards (media remotes, some mice)
/// report a handful of codes — often including some letters — but not the whole
/// alphabet *and* the space bar.
const TYPING_KEYS: &[evdev::KeyCode] = &[
    evdev::KeyCode::KEY_A,
    evdev::KeyCode::KEY_B,
    evdev::KeyCode::KEY_C,
    evdev::KeyCode::KEY_D,
    evdev::KeyCode::KEY_E,
    evdev::KeyCode::KEY_F,
    evdev::KeyCode::KEY_G,
    evdev::KeyCode::KEY_H,
    evdev::KeyCode::KEY_I,
    evdev::KeyCode::KEY_J,
    evdev::KeyCode::KEY_K,
    evdev::KeyCode::KEY_L,
    evdev::KeyCode::KEY_M,
    evdev::KeyCode::KEY_N,
    evdev::KeyCode::KEY_O,
    evdev::KeyCode::KEY_P,
    evdev::KeyCode::KEY_Q,
    evdev::KeyCode::KEY_R,
    evdev::KeyCode::KEY_S,
    evdev::KeyCode::KEY_T,
    evdev::KeyCode::KEY_U,
    evdev::KeyCode::KEY_V,
    evdev::KeyCode::KEY_W,
    evdev::KeyCode::KEY_X,
    evdev::KeyCode::KEY_Y,
    evdev::KeyCode::KEY_Z,
    evdev::KeyCode::KEY_SPACE,
];

/// Advice for anything which comes down to `/dev/input` permissions. Advice
/// arrays must be `&'static`, so the actionable specifics (the exact `usermod`
/// invocation, the path we failed on) live in the message instead.
const PERMISSION_ADVICE: &[&str] = &[
    "You will need to log out and back in (or reboot) before a new group membership takes effect.",
    "See the permissions guide at https://sierrasoftworks.github.io/voice-rs/guide/permissions.html — the same 'input' group also covers the /dev/uinput device we type through.",
];

/// Advice for "we looked, but nothing matched".
const NO_MATCH_ADVICE: &[&str] = &[
    "Set 'hotkey.device' to 'auto' to use the first device which reports your hotkey, to part of a device name, or to an exact '/dev/input/eventN' path.",
    "Run 'sudo evtest' and press your hotkey to see which device reports it.",
    "See the hotkey options reference at https://sierrasoftworks.github.io/voice-rs/profiles/#hotkey for the accepted values.",
];

/// An I/O failure against an evdev path, paired with the path so the resulting
/// human error can name it. Permission problems here are the single most
/// common first-run failure, so they get a dedicated message.
struct DeviceAccessError {
    path: PathBuf,
    source: std::io::Error,
}

impl DeviceAccessError {
    fn new(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self {
            path: path.into(),
            source,
        }
    }
}

impl HumanizableError for DeviceAccessError {
    fn to_human_error(self) -> crate::Error {
        match self.source.kind() {
            std::io::ErrorKind::PermissionDenied => human_errors::wrap_user(
                self.source,
                format!(
                    "We were not allowed to read {}, which is where we watch for your listen hotkey. Add yourself to the 'input' group by running: sudo usermod -aG input $USER",
                    self.path.display()
                ),
                PERMISSION_ADVICE,
            ),
            std::io::ErrorKind::NotFound => human_errors::wrap_user(
                self.source,
                format!(
                    "We could not find the input device {}, so we have no way to watch for your listen hotkey.",
                    self.path.display()
                ),
                &[
                    "Check the 'hotkey.device' setting in your profile — device numbers under /dev/input change when hardware is unplugged and replugged.",
                    "Set 'hotkey.device' to 'auto' (or to part of the device's name) so that we find it wherever it lands.",
                ],
            ),
            _ => human_errors::wrap_system(
                self.source,
                format!(
                    "We could not open the input device {} to watch for your listen hotkey.",
                    self.path.display()
                ),
                &["Please report this issue on GitHub so that we can investigate."],
            ),
        }
    }
}

/// Resolves the `hotkey.device` hint to an open evdev device which we can read
/// the configured hotkey from.
pub fn discover_device(hint: &str, key: KeyCode) -> Result<evdev::Device, crate::Error> {
    // A device-name substring can never start with '/', so a leading slash
    // unambiguously means "this is a path, open exactly this".
    if hint.starts_with('/') {
        return open_path(Path::new(hint), key);
    }

    let auto = hint.eq_ignore_ascii_case(AUTO);
    let wanted = hint.to_lowercase();

    // Every device we could read, for the "nothing matched" error; every device
    // which matched, in the same device-number order; and, for each of those,
    // how much it looks like a keyboard.
    let mut candidates = Vec::new();
    let mut matches: Vec<(PathBuf, String, evdev::Device)> = Vec::new();
    let mut ranks: Vec<Rank> = Vec::new();

    for (path, device) in enumerate_devices()? {
        let name = device.name().unwrap_or("<unnamed device>").to_string();
        candidates.push(name.clone());

        if (auto || name.to_lowercase().contains(&wanted)) && supports_key(&device, key) {
            ranks.push(rank(&supported_key_codes(&device)));
            matches.push((path, name, device));
        }
    }

    let Some(index) = preferred(&ranks) else {
        return Err(no_match_error(hint, auto, key, &candidates));
    };

    let (path, name, device) = matches.swap_remove(index);
    if ranks[index] == Rank::Keyboard {
        info!(
            "Watching for the listen hotkey on '{}' ({}).",
            name,
            path.display()
        );
    } else {
        // It reports the key, so it may well be the right answer — a keyboard
        // which under-reports its capabilities lands here too — but it is also
        // where the false positives live, so say what we settled on and why.
        warn!(
            "No device which reports your listen hotkey looks like a full keyboard; watching '{}' ({}) because it reports the key. If your hotkey does nothing, set 'hotkey.device' to part of your keyboard's name.",
            name,
            path.display()
        );
    }

    Ok(device)
}

/// One readable `/dev/input/event*` device, as `voice-orders devices` lists it.
///
/// Produced by [`list_devices`] and consumed by the listing's pure renderer, so
/// that what is printed can be exercised without any hardware at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedDevice {
    /// The device node, e.g. `/dev/input/event3`.
    pub path: PathBuf,
    /// The device's name — the string `hotkey.device` substring-matches.
    pub name: String,
    /// How much this looks like the keyboard a person types at.
    pub rank: Rank,
    /// Whether it reports the key the listing was asked about, which is what
    /// makes it a candidate for `device: auto` in the first place.
    pub reports_key: bool,
}

/// Every `/dev/input/event*` device we are allowed to read, in device-number
/// order, ranked exactly as [`discover_device`] ranks them.
///
/// The `key` is what `reports_key` is about: `auto` only ever considers devices
/// which report the hotkey, so a listing which did not say which devices those
/// are could not explain the choice it is describing.
pub fn list_devices(key: KeyCode) -> Result<Vec<ListedDevice>, crate::Error> {
    Ok(enumerate_devices()?
        .into_iter()
        .map(|(path, device)| ListedDevice {
            path,
            name: device.name().unwrap_or("<unnamed device>").to_string(),
            rank: rank(&supported_key_codes(&device)),
            reports_key: supports_key(&device, key),
        })
        .collect())
}

/// Which of a listing's devices `device: auto` would settle on, or `None` when
/// nothing reports the key.
///
/// The same [`preferred`] ranking `discover_device` uses, applied to the same
/// candidates, so the listing cannot claim a device the real run would not pick.
pub fn auto_choice(devices: &[ListedDevice]) -> Option<usize> {
    let candidates: Vec<(usize, Rank)> = devices
        .iter()
        .enumerate()
        .filter(|(_, device)| device.reports_key)
        .map(|(index, device)| (index, device.rank))
        .collect();

    let ranks: Vec<Rank> = candidates.iter().map(|(_, rank)| *rank).collect();

    preferred(&ranks).map(|chosen| candidates[chosen].0)
}

/// How much a device looks like the keyboard a person is typing at. Lower is
/// better, and `Ord` is what does the ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rank {
    /// It can type, and it advertises the extended keys only a device the
    /// kernel drives as a keyboard has.
    Keyboard,
    /// It can type, but reports nothing beyond the HID boot-keyboard set —
    /// a security key, a scanner, or a very plain keyboard.
    Types,
    /// It reports the hotkey and nothing else recommends it.
    ReportsTheKey,
}

impl Rank {
    /// How the rank reads in a device listing. Deliberately says nothing about
    /// any particular key — this is a judgement about the device itself.
    pub fn describe(self) -> &'static str {
        match self {
            Rank::Keyboard => "keyboard",
            Rank::Types => "types (boot-keyboard set only)",
            Rank::ReportsTheKey => "not a keyboard",
        }
    }
}

/// Picks which of the devices reporting the hotkey we should watch, given their
/// ranks in device-number order.
///
/// The best rank wins; ties go to the lowest event number, because that is the
/// order [`enumerate_devices`] produces and it is what makes the answer stable
/// across runs. An empty slice means nothing reported the key at all.
fn preferred(ranks: &[Rank]) -> Option<usize> {
    ranks
        .iter()
        .enumerate()
        .min_by_key(|(index, rank)| (**rank, *index))
        .map(|(index, _)| index)
}

/// Ranks a device by its supported key codes alone.
///
/// Pure over the code set so that the ranking can be exercised without any
/// hardware: see [`TYPING_KEYS`] and [`EXTENDED_KEY_FLOOR`] for what each bar
/// is and why it is where it is.
fn rank(supported: &BTreeSet<u16>) -> Rank {
    if !is_typing_keyboard(supported) {
        return Rank::ReportsTheKey;
    }

    match supported.last() {
        Some(&highest) if highest >= EXTENDED_KEY_FLOOR => Rank::Keyboard,
        _ => Rank::Types,
    }
}

/// Whether a set of supported key codes is that of something a person can type
/// a sentence on.
fn is_typing_keyboard(supported: &BTreeSet<u16>) -> bool {
    TYPING_KEYS
        .iter()
        .all(|key| supported.contains(&key.code()))
}

/// A device's `EV_KEY` capability set as raw codes.
fn supported_key_codes(device: &evdev::Device) -> BTreeSet<u16> {
    device
        .supported_keys()
        .map(|keys| keys.iter().map(|key| key.code()).collect())
        .unwrap_or_default()
}

/// Opens an explicit `/dev/input/eventN` path. We honour the profile exactly —
/// a device which doesn't advertise the key is still opened, since some
/// devices under-report their capabilities — but we say so loudly.
fn open_path(path: &Path, key: KeyCode) -> Result<evdev::Device, crate::Error> {
    let device =
        evdev::Device::open(path).map_err(|e| DeviceAccessError::new(path, e).to_human_error())?;

    let name = device.name().unwrap_or("<unnamed device>").to_string();
    if supports_key(&device, key) {
        info!(
            "Watching for the listen hotkey on '{}' ({}).",
            name,
            path.display()
        );
    } else {
        warn!(
            "The device '{}' ({}) does not report the hotkey you configured (evdev code {}); we will watch it anyway, but the hotkey may never fire.",
            name,
            path.display(),
            key.0
        );
    }

    Ok(device)
}

/// Whether a device advertises the configured key in its `EV_KEY` capability
/// set. Devices with no key capability at all (mice, touchpads) answer `false`.
fn supports_key(device: &evdev::Device, key: KeyCode) -> bool {
    device
        .supported_keys()
        .is_some_and(|keys| keys.contains(evdev::KeyCode::new(key.0)))
}

/// Opens every `/dev/input/event*` node we are allowed to, in device-number
/// order so that "the first device which..." is stable across runs.
///
/// `evdev::enumerate()` silently swallows failures, which turns the single
/// most common first-run problem (not being in the `input` group) into a
/// baffling "no device matched". We enumerate ourselves so that a run in which
/// *everything* was refused reports the permission problem instead.
fn enumerate_devices() -> Result<Vec<(PathBuf, evdev::Device)>, crate::Error> {
    let entries = std::fs::read_dir(INPUT_DIR)
        .map_err(|e| DeviceAccessError::new(INPUT_DIR, e).to_human_error())?;

    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("event"))
        })
        .collect();
    paths.sort_by_key(|path| event_number(path));

    let mut devices = Vec::with_capacity(paths.len());
    let mut denied: Option<(PathBuf, std::io::Error)> = None;

    for path in paths {
        match evdev::Device::open(&path) {
            Ok(device) => devices.push((path, device)),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                debug!("We were not allowed to open {}.", path.display());
                denied.get_or_insert((path, e));
            }
            Err(e) => debug!("We could not open {}: {}", path.display(), e),
        }
    }

    // Some nodes always refuse us (and that's fine) — but if *none* of them
    // opened and at least one refused, this is the permissions problem.
    if devices.is_empty()
        && let Some((path, e)) = denied
    {
        return Err(DeviceAccessError::new(path, e).to_human_error());
    }

    Ok(devices)
}

/// The device number in `/dev/input/eventN`, for numeric ordering (so that
/// `event2` sorts before `event10`, unlike the lexicographic path order).
fn event_number(path: &Path) -> u32 {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("event"))
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(u32::MAX)
}

/// Builds the "nothing matched" error, naming every device we could see so
/// that the fix (copying a name into `hotkey.device`) is one step away.
fn no_match_error(hint: &str, auto: bool, key: KeyCode, candidates: &[String]) -> crate::Error {
    let seen = if candidates.is_empty() {
        "We could not read any input devices at all.".to_string()
    } else {
        format!(
            "The devices we could read are: {}.",
            candidates
                .iter()
                .map(|name| format!("'{name}'"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    let what = if auto {
        format!(
            "We could not find any input device which reports the hotkey you configured (evdev code {}).",
            key.0
        )
    } else {
        format!(
            "We could not find an input device whose name contains '{}' and which reports the hotkey you configured (evdev code {}).",
            hint, key.0
        )
    };

    human_errors::user(format!("{what} {seen}"), NO_MATCH_ADVICE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("/dev/input/event0", 0)]
    #[case("/dev/input/event2", 2)]
    #[case("/dev/input/event10", 10)]
    #[case("/dev/input/mice", u32::MAX)]
    #[case("/dev/input/eventX", u32::MAX)]
    fn test_event_number(#[case] path: &str, #[case] expected: u32) {
        assert_eq!(event_number(Path::new(path)), expected);
    }

    #[test]
    fn test_event_number_orders_numerically() {
        let mut paths: Vec<PathBuf> = ["event10", "event2", "event1"]
            .iter()
            .map(|name| Path::new(INPUT_DIR).join(name))
            .collect();
        paths.sort_by_key(|path| event_number(path));

        assert_eq!(
            paths
                .iter()
                .map(|p| p.file_name().unwrap().to_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["event1", "event2", "event10"]
        );
    }

    /// The whole HID boot-keyboard set and nothing above it: every typing key,
    /// the modifiers a hotkey is usually bound to, and `KEY_F12` — which is the
    /// highest code a device in boot-keyboard mode reports.
    ///
    /// This is measured from a real `Yubico YubiKey OTP+FIDO+CCID`, whose 105
    /// codes are a strict subset of every real keyboard on the same machine and
    /// top out at 127.
    fn boot_keyboard_keys() -> BTreeSet<u16> {
        TYPING_KEYS
            .iter()
            .map(|key| key.code())
            .chain([
                evdev::KeyCode::KEY_LEFTCTRL.code(),
                evdev::KeyCode::KEY_RIGHTCTRL.code(),
                evdev::KeyCode::KEY_ENTER.code(),
                evdev::KeyCode::KEY_F12.code(),
            ])
            .collect()
    }

    /// A real keyboard: the boot set, plus the extended keys the kernel exposes
    /// for a device it drives as a keyboard.
    fn keyboard_keys() -> BTreeSet<u16> {
        let mut keys = boot_keyboard_keys();
        keys.insert(evdev::KeyCode::KEY_F13.code());
        keys.insert(evdev::KeyCode::KEY_PLAYPAUSE.code());
        keys
    }

    /// A mouse with a couple of extra buttons: nothing anybody types on.
    fn mouse_keys() -> BTreeSet<u16> {
        [
            evdev::KeyCode::BTN_LEFT.code(),
            evdev::KeyCode::BTN_RIGHT.code(),
            evdev::KeyCode::KEY_LEFTCTRL.code(),
            evdev::KeyCode::KEY_A.code(),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn test_a_full_keyboard_outranks_everything() {
        assert!(is_typing_keyboard(&keyboard_keys()));
        assert_eq!(rank(&keyboard_keys()), Rank::Keyboard);
    }

    #[test]
    fn test_a_security_key_can_type_but_is_not_a_keyboard() {
        // This is the device which used to win `device: auto`: a YubiKey types
        // its one-time passwords through a standard boot keyboard, so it really
        // does report the whole alphabet, the space bar and both controls. What
        // it does not report is anything above the boot set.
        let keys = boot_keyboard_keys();

        assert!(
            is_typing_keyboard(&keys),
            "it genuinely can type, which is exactly why the alphabet alone cannot separate it"
        );
        assert_eq!(rank(&keys), Rank::Types);
        assert!(
            rank(&keyboard_keys()) < rank(&keys),
            "a real keyboard must outrank it"
        );
    }

    #[test]
    fn test_a_device_missing_one_letter_cannot_type() {
        let mut keys = keyboard_keys();
        keys.remove(&evdev::KeyCode::KEY_Q.code());

        assert!(!is_typing_keyboard(&keys));
        assert_eq!(
            rank(&keys),
            Rank::ReportsTheKey,
            "the extended keys do not make up for not being able to type"
        );
    }

    #[test]
    fn test_a_mouse_which_reports_the_hotkey_ranks_last() {
        assert_eq!(rank(&mouse_keys()), Rank::ReportsTheKey);
    }

    #[test]
    fn test_a_device_with_no_keys_at_all_ranks_last() {
        assert_eq!(rank(&BTreeSet::new()), Rank::ReportsTheKey);
    }

    #[rstest]
    #[case::nothing_matched(&[], None)]
    // The historical behaviour, when nothing is better than anything else: the
    // first device which reports the key at all.
    #[case::no_keyboards(&[Rank::ReportsTheKey, Rank::ReportsTheKey], Some(0))]
    // The real-run bug: a security key enumerates before the keyboard.
    #[case::keyboard_after_a_security_key(&[Rank::Types, Rank::Keyboard], Some(1))]
    // Several keyboards: the lowest event number still wins.
    #[case::first_keyboard_wins(&[Rank::Types, Rank::Keyboard, Rank::Keyboard], Some(1))]
    #[case::already_first(&[Rank::Keyboard, Rank::Types], Some(0))]
    // Nothing is a full keyboard, so something which can at least type wins.
    #[case::typing_beats_reporting(&[Rank::ReportsTheKey, Rank::Types], Some(1))]
    // Every rank at once: the best still wins, wherever it sits.
    #[case::the_best_wins(&[Rank::ReportsTheKey, Rank::Types, Rank::Keyboard], Some(2))]
    fn test_a_real_keyboard_outranks_anything_which_merely_reports_the_key(
        #[case] ranks: &[Rank],
        #[case] expected: Option<usize>,
    ) {
        assert_eq!(preferred(ranks), expected);
    }

    fn listed(event: u32, rank: Rank, reports_key: bool) -> ListedDevice {
        ListedDevice {
            path: PathBuf::from(format!("{INPUT_DIR}/event{event}")),
            name: format!("device {event}"),
            rank,
            reports_key,
        }
    }

    #[test]
    fn test_auto_choice_ignores_devices_which_do_not_report_the_key() {
        // The listing shows every readable device; `auto` only ever considers
        // the ones which report the key, however good the others look.
        let devices = [
            listed(0, Rank::Keyboard, false),
            listed(1, Rank::Types, true),
            listed(2, Rank::Keyboard, true),
        ];

        assert_eq!(auto_choice(&devices), Some(2));
    }

    #[test]
    fn test_auto_choice_agrees_with_the_ranking() {
        // The same case `preferred` is tested on, through the listing's eyes:
        // a security key which enumerates before the keyboard.
        let devices = [
            listed(0, Rank::Types, true),
            listed(1, Rank::Keyboard, true),
        ];

        assert_eq!(auto_choice(&devices), Some(1));
    }

    #[test]
    fn test_auto_choice_has_nothing_to_pick_when_nothing_reports_the_key() {
        assert_eq!(auto_choice(&[listed(0, Rank::Keyboard, false)]), None);
        assert_eq!(auto_choice(&[]), None);
    }

    #[rstest]
    #[case(Rank::Keyboard, "keyboard")]
    #[case(Rank::Types, "types (boot-keyboard set only)")]
    #[case(Rank::ReportsTheKey, "not a keyboard")]
    fn test_every_rank_has_something_to_say_about_itself(
        #[case] rank: Rank,
        #[case] expected: &str,
    ) {
        assert_eq!(rank.describe(), expected);
    }

    #[test]
    fn test_permission_error_names_the_group_command() {
        let err = DeviceAccessError::new(
            "/dev/input/event3",
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        )
        .to_human_error();

        let message = err.message();
        assert!(
            message.contains("sudo usermod -aG input $USER"),
            "the permission error should tell you exactly what to run, got: {message}"
        );
        assert!(
            message.contains("/dev/input/event3"),
            "the permission error should name the path, got: {message}"
        );
        assert!(
            err.is(human_errors::Kind::User),
            "a missing group membership is the user's to fix, not a system fault"
        );
        assert!(
            err.advice()
                .iter()
                .any(|tip| tip.contains("sierrasoftworks.github.io")),
            "the permission error should link to the docs, got: {:?}",
            err.advice()
        );
    }

    #[test]
    fn test_missing_device_error_is_actionable() {
        let err = DeviceAccessError::new(
            "/dev/input/event99",
            std::io::Error::from(std::io::ErrorKind::NotFound),
        )
        .to_human_error();

        assert!(err.is(human_errors::Kind::User));
        assert!(
            err.description().contains("/dev/input/event99"),
            "the error should name the device we could not find"
        );
    }

    #[test]
    fn test_no_match_error_lists_candidates() {
        let err = no_match_error(
            "razer",
            false,
            KeyCode(97),
            &[
                "AT Translated Set 2 keyboard".to_string(),
                "Logitech USB Receiver".to_string(),
            ],
        );

        let message = err.description();
        assert!(
            message.contains("razer"),
            "the error should echo the hint, got: {message}"
        );
        assert!(
            message.contains("'AT Translated Set 2 keyboard'")
                && message.contains("'Logitech USB Receiver'"),
            "the error should list every device we could see, got: {message}"
        );
        assert!(
            message.contains("97"),
            "the error should name the key code we were looking for, got: {message}"
        );
        assert!(err.is(human_errors::Kind::User));
    }

    #[test]
    fn test_no_match_error_for_auto_does_not_quote_the_hint() {
        let err = no_match_error("auto", true, KeyCode(97), &[]);

        let message = err.description();
        assert!(
            !message.contains("'auto'"),
            "'auto' is not a device name, so it should not be reported as one: {message}"
        );
        assert!(
            message.contains("could not read any input devices"),
            "with nothing enumerated we should say so, got: {message}"
        );
    }

    /// The listing against real hardware: it must agree with discovery about
    /// which device `auto` settles on.
    #[test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    fn test_list_devices_agrees_with_discovery() {
        const KEY_LEFTCTRL: u16 = 29;

        let Ok(devices) = list_devices(KeyCode(KEY_LEFTCTRL)) else {
            return;
        };

        match (
            auto_choice(&devices),
            discover_device(AUTO, KeyCode(KEY_LEFTCTRL)),
        ) {
            (Some(index), Ok(device)) => assert_eq!(
                devices[index].name,
                device.name().unwrap_or("<unnamed device>"),
                "the listing must name the device a real run would watch"
            ),
            (None, Ok(device)) => panic!(
                "discovery found '{}' but the listing marked nothing",
                device.name().unwrap_or("<unnamed device>")
            ),
            // Nothing reports the key (a container, say): both agree there is
            // nothing to pick.
            (choice, Err(_)) => assert_eq!(choice, None),
        }
    }

    /// Enumeration touches real `/dev/input` nodes, so it needs a Linux box
    /// with the current user in the `input` group.
    #[test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    fn test_enumerate_devices() {
        let devices = enumerate_devices().expect("we should be able to enumerate input devices");

        for (path, device) in &devices {
            assert!(
                path.to_string_lossy().starts_with("/dev/input/event"),
                "we should only enumerate event nodes, got {}",
                path.display()
            );
            // Touching the name proves the ioctls behind the device worked.
            let _ = device.name();
        }
    }

    /// Smoke test for the `auto` path: opens the first device reporting the
    /// left control key, which any real keyboard has.
    #[test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    fn test_discover_device_auto() {
        const KEY_LEFTCTRL: u16 = 29;

        match discover_device(AUTO, KeyCode(KEY_LEFTCTRL)) {
            Ok(device) => assert!(
                supports_key(&device, KeyCode(KEY_LEFTCTRL)),
                "auto discovery must only return devices which report the key"
            ),
            // A machine with no keyboard (a CI container) is a legitimate
            // outcome here; a permission problem is not something this test
            // can fix either. Either way the error must be human.
            Err(e) => assert!(
                e.is(human_errors::Kind::User),
                "discovery failures should be user errors: {}",
                e.message()
            ),
        }
    }

    /// The ranking against real hardware: whatever the best-ranked device on
    /// this machine is, that is the one `auto` must settle on.
    #[test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    fn test_discover_device_auto_prefers_a_real_keyboard() {
        const KEY_LEFTCTRL: u16 = 29;

        let Ok(devices) = enumerate_devices() else {
            return;
        };

        let Some(best) = devices
            .iter()
            .filter(|(_, device)| supports_key(device, KeyCode(KEY_LEFTCTRL)))
            .map(|(_, device)| rank(&supported_key_codes(device)))
            .min()
        else {
            eprintln!("skipping: nothing on this machine reports left control");
            return;
        };

        let device = discover_device(AUTO, KeyCode(KEY_LEFTCTRL))
            .expect("a machine with a keyboard should discover one");
        assert_eq!(
            rank(&supported_key_codes(&device)),
            best,
            "'{}' was chosen, but something better-ranked was available",
            device.name().unwrap_or("<unnamed device>")
        );
    }

    #[test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    fn test_discover_device_missing_path() {
        let err = discover_device("/dev/input/event9999", KeyCode(29))
            .expect_err("a device which does not exist should not be discoverable");

        assert!(err.description().contains("/dev/input/event9999"));
    }
}
