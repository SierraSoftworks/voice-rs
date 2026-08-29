//! `voice-orders doctor [profile]`: read-only diagnosis of everything the
//! kernel-level approach needs. See DESIGN.md §"`setup` and `doctor`".
//!
//! Two rules shape the whole module. The first is **run every check**: a
//! missing model must not hide a missing udev rule, so nothing here returns
//! early and the exit code is derived from the finished report rather than from
//! the first thing which went wrong — exactly as `validate` does it.
//!
//! The second is that every check is a function returning a [`CheckResult`],
//! which is what lets `setup` reuse the diagnosis rather than re-deriving it,
//! and what lets the rendering be tested without any of the hardware the checks
//! themselves touch.

use std::path::{Path, PathBuf};

use clap::Args;
use cpal::traits::{DeviceTrait, HostTrait};
use tracing_batteries::prelude::*;

use crate::audio;
use crate::config::{
    MODEL_PATH_ENV, Profile, ResolvedSettings, SystemConfig, loader, resolve_model, system,
};
#[cfg(target_os = "linux")]
use crate::hotkey::discover_device;
use crate::output::KeyCode;
#[cfg(target_os = "linux")]
use crate::output::UinputSink;
use crate::output::keys;
#[cfg(not(target_os = "linux"))]
use crate::output::{KeySink, PlatformSink};
use crate::recognition::libvosk;

/// The device node the virtual keyboard is created on.
#[cfg(target_os = "linux")]
pub(crate) const UINPUT_PATH: &str = "/dev/uinput";

/// Where the kernel exposes evdev device nodes.
#[cfg(target_os = "linux")]
const INPUT_DIR: &str = "/dev/input";

/// The group which owns both `/dev/uinput` and `/dev/input/event*`.
#[cfg(target_os = "linux")]
pub(crate) const INPUT_GROUP: &str = "input";

/// The group database we read configured membership from.
#[cfg(target_os = "linux")]
pub(crate) const GROUP_FILE: &str = "/etc/group";

/// The name the doctor's throwaway virtual keyboard is created under, so that
/// it cannot be confused with a running `voice-orders run`.
#[cfg(target_os = "linux")]
const PROBE_DEVICE_NAME: &str = "voice-orders-doctor";

/// The `hotkey.device` value which asks discovery to pick a device for us.
#[cfg(target_os = "linux")]
const AUTO_DEVICE: &str = "auto";

/// The key we ask discovery to find a device for when there is no profile to
/// tell us which one matters: every keyboard has a left control.
#[cfg(target_os = "linux")]
const PROBE_KEY: &str = "leftctrl";

/// The relative path which distinguishes a dynamic-graph model (which can be
/// constrained to a grammar) from a precompiled static one (which cannot).
const DYNAMIC_GRAPH: &str = "graph/Gr.fst";

/// Advice for everything `voice-orders setup` knows how to fix.
#[cfg(target_os = "linux")]
const SETUP_ADVICE: &[&str] = &[
    "Run 'voice-orders setup' to apply the missing system configuration, or 'voice-orders setup --print' to see the commands and run them yourself.",
    "See the permissions guide at https://sierrasoftworks.github.io/voice-rs/guide/permissions.html for what each step does.",
];

/// Advice for a group membership which is configured but not yet in effect.
#[cfg(target_os = "linux")]
const RELOGIN_ADVICE: &[&str] = &[
    "Log out and back in (or reboot) — group membership is attached to your session when you log in, so a shell or desktop session which started earlier is still denied.",
    "'id -nG' will list 'input' once the change has taken effect.",
];

/// Advice for a model which resolves but cannot be constrained to a grammar.
const STATIC_GRAPH_ADVICE: &[&str] = &[
    "Download vosk-model-en-us-0.22-lgraph from https://alphacephei.com/vosk/models and point at it with '--model <path>', with a 'model:' line in your profile, or by setting VOSK_MODEL_PATH.",
    "The large precompiled models (such as vosk-model-en-us-0.22) ship a static graph, which cannot be constrained to your profile's phrases.",
];

/// Advice for having no model at all, when there is no profile to blame.
const NO_MODEL_ADVICE: &[&str] = &[
    "Download vosk-model-en-us-0.22-lgraph from https://alphacephei.com/vosk/models, unpack it, and point at it with '--model <path>' or by setting VOSK_MODEL_PATH.",
    "It must be a model with a dynamic graph (one containing 'graph/Gr.fst'): voice-orders constrains recognition to your profile's phrases, which the large precompiled models cannot do.",
];

#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// A profile to check as well: it must load, and its hotkey device must
    /// resolve. Its `model:` field is used when `--model` is not given.
    pub profile: Option<String>,

    /// The Vosk model to check.
    /// Overrides the profile's `model:` field and $VOSK_MODEL_PATH.
    #[arg(long)]
    pub model: Option<PathBuf>,
}

/// The verdict of one check: what we found, and what to do about it.
///
/// `advice` is a `Vec<String>` rather than the `&'static [&'static str]` the
/// house error style uses, because most of it is borrowed straight from a
/// `human_errors::Error` we have already built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckResult {
    /// Whether this check passed.
    pub ok: bool,
    /// What is right (when `ok`) or wrong (when not), in one sentence.
    pub headline: String,
    /// What to do about it. Only rendered for a failing check.
    pub advice: Vec<String>,
}

impl CheckResult {
    /// A check which passed. Passing checks say what is *right*, so that a
    /// clean report still tells you which microphone and model you are on.
    pub(crate) fn ok(headline: impl Into<String>) -> Self {
        Self {
            ok: true,
            headline: headline.into(),
            advice: Vec::new(),
        }
    }

    /// A check which failed, with the advice to fix it.
    pub(crate) fn failed<S: AsRef<str>>(headline: impl Into<String>, advice: &[S]) -> Self {
        Self {
            ok: false,
            headline: headline.into(),
            advice: advice.iter().map(|tip| tip.as_ref().to_string()).collect(),
        }
    }

    /// A check which failed because of an error we have already humanized:
    /// its message becomes the headline and its advice comes along unchanged.
    pub(crate) fn from_error(error: &crate::Error) -> Self {
        Self::failed(error.description(), &error.advice())
    }

    /// The check as it is printed: a `✓`/`✗` line, then indented advice.
    fn render(&self) -> String {
        let mut out = String::new();

        out.push_str(if self.ok { "✓ " } else { "✗ " });
        out.push_str(&self.headline);
        out.push('\n');

        if !self.ok {
            for tip in &self.advice {
                out.push_str("    ");
                out.push_str(tip);
                out.push('\n');
            }
        }

        out
    }
}

/// Diagnoses this machine, printing the report and returning the exit code.
pub async fn run(args: DoctorArgs) -> Result<i32, crate::Error> {
    // This machine's configuration comes first and is *not* a check: every
    // check below reports what it resolves to, so a file we cannot understand
    // would make the whole report a lie rather than one more failing line.
    let system = SystemConfig::load()?;

    // The profile is loaded before anything else because check 6 needs its
    // `model:` field — but a profile which does not load is check 7's problem,
    // not a reason to stop, so the failure travels into the report.
    let loaded = match args.profile.as_deref() {
        Some(source) => Some(load_profile(source).await),
        None => None,
    };
    let profile = loaded.as_ref().and_then(|result| result.as_ref().ok());

    // What the profile and the machine add up to. Without a profile there is
    // nothing to merge, and the machine's own values stand alone.
    let settings = profile.map(|profile| ResolvedSettings::resolve(profile, &system));

    println!("{}\n", system_summary(&system));

    // Checks 1–3 are about how this platform gets keys in and out of the
    // kernel, which is the one part of the diagnosis with nothing in common
    // between Linux and Windows.
    let mut results = platform_checks().await;
    results.extend([
        check_audio_input(audio_device(settings.as_ref(), &system)),
        check_libvosk(),
        check_model(resolve(args.model.as_deref(), profile, &system)),
    ]);

    if let Some(loaded) = &loaded {
        results.push(check_profile(loaded, settings.as_ref()));
    }

    print!("{}", render(&results));

    let failures = results.iter().filter(|result| !result.ok).count();
    let code = i32::from(failures > 0);
    debug!(
        checks = results.len(),
        failures, "Diagnosis finished with exit code {code}."
    );

    Ok(code)
}

/// Renders a finished report: every check, then the summary.
fn render(results: &[CheckResult]) -> String {
    let mut out = String::new();

    for result in results {
        out.push_str(&result.render());
    }

    let checks = results.len();
    let failures = results.iter().filter(|result| !result.ok).count();
    out.push('\n');
    out.push_str(&format!(
        "{checks} {} — {failures} failed.\n",
        if checks == 1 { "check" } else { "checks" }
    ));

    out
}

/// Loads and parses a profile, keeping the failure rather than propagating it.
async fn load_profile(source: &str) -> Result<Profile, crate::Error> {
    Profile::parse(&loader::load(source).await?)
}

/// The line above the checks: whether this machine has a configuration file at
/// all, and where we looked for it either way. Not a check — nothing here can
/// fail without [`SystemConfig::load`] having failed first — but every check
/// below is reported through it, so it belongs at the top of the report.
fn system_summary(system: &SystemConfig) -> String {
    match &system.source {
        Some(path) => format!("System configuration: {}.", path.display()),
        None => format!(
            "System configuration: none at {} — the built-in defaults apply.",
            system::config_path().display()
        ),
    }
}

/// The microphone the checks should be reporting on: whatever the profile and
/// the machine resolved to, or — with no profile, or one which did not load —
/// the machine's own default.
fn audio_device<'a>(
    settings: Option<&'a Result<ResolvedSettings, crate::Error>>,
    system: &'a SystemConfig,
) -> &'a str {
    match settings {
        Some(Ok(settings)) => &settings.audio_device,
        _ => system.audio_device(),
    }
}

/// Works out which model we should be checking.
///
/// With a profile this is exactly [`resolve_model`]; without one there is no
/// `model:` field to consult, so only `--model` and `$VOSK_MODEL_PATH` remain
/// — the first of which may still be a bare model name.
fn resolve(
    cli: Option<&Path>,
    profile: Option<&Profile>,
    system: &SystemConfig,
) -> Result<PathBuf, crate::Error> {
    if let Some(profile) = profile {
        return resolve_model(cli, profile, system);
    }

    if let Some(path) = cli {
        return Ok(system::expand_model(path, system));
    }

    std::env::var_os(MODEL_PATH_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| {
            human_errors::user(
                format!(
                    "We do not know which speech model to check: it was not given with '--model', no profile was named on the command line, and the {MODEL_PATH_ENV} environment variable is not set."
                ),
                NO_MODEL_ADVICE,
            )
        })
}

// ── Checks 1–3: this platform's input plumbing ──────────────────────────────

/// Checks 1–3 on Linux: the `uinput` node, a virtual keyboard, and `input`
/// group membership with a readable `/dev/input/event*`.
#[cfg(target_os = "linux")]
async fn platform_checks() -> Vec<CheckResult> {
    vec![
        check_uinput_node(Path::new(UINPUT_PATH)),
        check_virtual_keyboard().await,
        check_input_access(),
    ]
}

/// Checks 1–3 on Windows.
///
/// The first is genuinely good news — nothing has to be configured, which is the
/// whole of what the Linux checks 1 and 3 are about. The other two follow the
/// same philosophy the Linux ones do: rather than inferring that keyboard output
/// and the hotkey would work, they *do* them — a keystroke nothing is bound to,
/// and a hook installed and immediately removed.
#[cfg(not(target_os = "linux"))]
async fn platform_checks() -> Vec<CheckResult> {
    vec![
        CheckResult::ok(
            "Windows needs no drivers, kernel modules or group membership for voice-orders: keyboard output goes through SendInput and the listen hotkey through a low-level keyboard hook.",
        ),
        check_send_input().await,
        check_keyboard_hook(),
    ]
}

// ── Check 2 (Windows): pressing a key ───────────────────────────────────────

/// Presses and releases a key nothing is bound to, through the real sink.
///
/// The Windows counterpart of creating a virtual keyboard: `SendInput` needs no
/// device, so the only thing which can go wrong is the thing worth reporting —
/// Windows refusing to deliver our keystrokes at all, which is what UIPI does to
/// a program trying to type into a window running at a higher integrity level.
/// `f24` is the probe for the same reason the Linux side uses it: no keyboard
/// has one and nothing is listening for it.
#[cfg(not(target_os = "linux"))]
async fn check_send_input() -> CheckResult {
    /// The key we press to prove we can press keys.
    const PROBE_KEY: &str = "f24";

    let key = keys::from_name(PROBE_KEY).expect("the probe key must be in the key table");

    let mut sink = match PlatformSink::new().await {
        Ok(sink) => sink,
        Err(e) => return CheckResult::from_error(&e),
    };

    if let Err(e) = sink.press(key).await {
        return CheckResult::from_error(&e);
    }

    if let Err(e) = sink.release(key).await {
        return CheckResult::from_error(&e);
    }

    CheckResult::ok(
        "Keyboard output works: a test keystroke was accepted by Windows, so voice-orders can press keys for you.",
    )
}

// ── Check 3 (Windows): watching the keyboard ────────────────────────────────

/// Installs the low-level keyboard hook the listen hotkey watches with, and
/// immediately removes it.
///
/// Like check 2, this is the real mechanism rather than an inference about it.
/// The report says plainly that the hook is system-wide and that it observes
/// rather than consumes, because both are things people reasonably worry about
/// before letting a program watch their keyboard — and it ends with this
/// process' integrity level, which decides which windows the hook can see keys
/// for at all.
#[cfg(not(target_os = "linux"))]
fn check_keyboard_hook() -> CheckResult {
    match crate::hotkey::probe_hook() {
        Ok(()) => CheckResult::ok(format!(
            "The listen hotkey can be watched for: a low-level keyboard hook installs, which sees your hotkey system-wide (including inside fullscreen games) and passes every key on to whatever you are running. {}",
            integrity_note(elevated())
        )),
        Err(e) => CheckResult::from_error(&e),
    }
}

/// What the report says about this process' integrity level.
///
/// Informational rather than a verdict, which is why the check passes either
/// way: running unelevated is the *right* way to run voice-orders, and the
/// caveat only bites for the minority of people whose game runs as
/// administrator. UIPI is symmetric — an ordinary process can neither type into
/// an elevated window (which check 2's advice covers) nor see keys pressed at
/// one — so the hotkey stops working over exactly the windows keystrokes stop
/// arriving at.
#[cfg(not(target_os = "linux"))]
fn integrity_note(elevated: Option<bool>) -> &'static str {
    match elevated {
        Some(true) => {
            "voice-orders is running elevated, so the hook sees keys pressed at every window."
        }
        Some(false) => {
            "voice-orders is running at the ordinary integrity level, which is how it should be run — but Windows will not show it keystrokes made to a window running as administrator, so if your game is elevated, start voice-orders as administrator too."
        }
        None => {
            "We could not tell whether voice-orders is running elevated; if your game runs as administrator and the hotkey does nothing, start voice-orders as administrator too."
        }
    }
}

/// Whether this process is running elevated, or [`None`] if Windows would not
/// tell us.
///
/// `TokenElevation` rather than a group check: it is the same question the
/// shield icon asks, it is a single `u32`, and it is true for the built-in
/// Administrator account (which has no split token) as well as for an elevated
/// ordinary one.
#[cfg(windows)]
fn elevated() -> Option<bool> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = std::ptr::null_mut();

    // SAFETY: the pseudo-handle `GetCurrentProcess` returns needs no closing,
    // and the token handle is written into storage we own (and closed below).
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return None;
    }

    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;

    // SAFETY: `TokenElevation` answers with a `TOKEN_ELEVATION`, which is what
    // we point it at and whose size is what we declare.
    let queried = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &raw mut returned,
        )
    };

    // SAFETY: the handle came from `OpenProcessToken`, is closed exactly once,
    // and is not used afterwards.
    unsafe { CloseHandle(token) };

    (queried != 0).then_some(elevation.TokenIsElevated != 0)
}

// ── Check 1: the device node ────────────────────────────────────────────────

/// Whether `/dev/uinput` exists at all — which is to say, whether the `uinput`
/// kernel module is loaded.
#[cfg(target_os = "linux")]
pub(crate) fn check_uinput_node(path: &Path) -> CheckResult {
    if path.exists() {
        return CheckResult::ok(format!("{} exists.", path.display()));
    }

    CheckResult::failed(
        format!(
            "{} does not exist, which means the 'uinput' kernel module is not loaded — voice-orders has nothing to create its virtual keyboard on.",
            path.display()
        ),
        SETUP_ADVICE,
    )
}

// ── Check 2: creating a virtual keyboard ────────────────────────────────────

#[cfg(target_os = "linux")]
/// Creates a virtual keyboard and immediately destroys it.
///
/// This is the definitive permissions test: everything else about `/dev/uinput`
/// — the node existing, the udev rule, the group — is inference about whether
/// this would work, and this simply does it.
pub(crate) async fn check_virtual_keyboard() -> CheckResult {
    match UinputSink::with_name(PROBE_DEVICE_NAME).await {
        Ok(sink) => {
            // Dropping the sink destroys the device; being explicit about it is
            // the whole point of the check.
            drop(sink);
            CheckResult::ok("A virtual keyboard can be created on /dev/uinput.")
        }
        Err(e) => CheckResult::from_error(&e),
    }
}

// ── Check 3: input group membership and readable devices ────────────────────

#[cfg(target_os = "linux")]
/// The `input` line of `/etc/group`, as far as we care about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InputGroup {
    /// The group's numeric id, which is what `getgroups(2)` reports.
    pub gid: u32,
    /// The users listed as secondary members of the group.
    pub members: Vec<String>,
}

#[cfg(target_os = "linux")]
/// How the `input` group stands for this user, right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupState {
    /// `/etc/group` has no `input` group at all.
    Missing,
    /// The group exists, but does not list this user.
    NotConfigured,
    /// The group lists this user, but this session predates that.
    Stale,
    /// The group lists this user and this session has it.
    Effective,
}

#[cfg(target_os = "linux")]
/// Parses the `input` line out of an `/etc/group` file.
///
/// The format is `name:password:gid:member,member,…`; anything which is not the
/// `input` group, or which does not have a numeric gid, is ignored.
pub(crate) fn parse_input_group(content: &str) -> Option<InputGroup> {
    content.lines().find_map(|line| {
        let mut fields = line.split(':');
        if fields.next()? != INPUT_GROUP {
            return None;
        }

        let _password = fields.next()?;
        let gid = fields.next()?.parse().ok()?;
        let members = fields
            .next()
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|member| !member.is_empty())
            .map(str::to_string)
            .collect();

        Some(InputGroup { gid, members })
    })
}

#[cfg(target_os = "linux")]
/// Whether `/etc/group` lists this user as a member of the `input` group —
/// what `setup` needs to know, since it is what `usermod -aG` changes.
pub(crate) fn is_configured(group: Option<&InputGroup>, username: &str) -> bool {
    group.is_some_and(|group| group.members.iter().any(|member| member == username))
}

#[cfg(target_os = "linux")]
/// Compares the configured membership against the one this session actually
/// has, which is the distinction people trip over: `usermod` takes effect at
/// the next login, not immediately.
pub(crate) fn group_state(
    group: Option<&InputGroup>,
    username: &str,
    effective: &[u32],
) -> GroupState {
    let Some(group) = group else {
        return GroupState::Missing;
    };

    if !is_configured(Some(group), username) {
        return GroupState::NotConfigured;
    }

    if effective.contains(&group.gid) {
        GroupState::Effective
    } else {
        GroupState::Stale
    }
}

#[cfg(target_os = "linux")]
/// The user we are configuring, from `$USER` and then `$LOGNAME`.
pub(crate) fn current_username() -> Result<String, crate::Error> {
    for variable in ["USER", "LOGNAME"] {
        if let Some(value) = std::env::var_os(variable)
            && !value.is_empty()
        {
            return Ok(value.to_string_lossy().into_owned());
        }
    }

    Err(human_errors::user(
        "We could not work out which user you are: neither $USER nor $LOGNAME is set in this environment.",
        &[
            "Set USER to your login name before running this command, e.g. 'USER=$(id -un) voice-orders doctor'.",
        ],
    ))
}

#[cfg(target_os = "linux")]
/// The group ids this process actually carries — the *effective* membership,
/// as opposed to the one `/etc/group` has been configured with.
pub(crate) fn effective_gids() -> Vec<u32> {
    // SAFETY: `getgroups(2)` with a zero size writes nothing and only reports
    // how many entries there are, which is how the buffer below gets sized.
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };

    let mut gids: Vec<libc::gid_t> = if count > 0 {
        let mut buffer = vec![0 as libc::gid_t; count as usize];
        // SAFETY: the buffer has room for `count` entries, which is exactly
        // what we are telling the kernel it may write.
        let written = unsafe { libc::getgroups(count, buffer.as_mut_ptr()) };
        if written < 0 {
            debug!("getgroups(2) refused to report our supplementary groups.");
            Vec::new()
        } else {
            buffer.truncate(written as usize);
            buffer
        }
    } else {
        Vec::new()
    };

    // Linux is not required to include the primary group in `getgroups`, and a
    // machine whose users' primary group *is* `input` is perfectly legal.
    // SAFETY: neither call takes arguments nor can fail; both read only the
    // calling process's own credentials.
    gids.push(unsafe { libc::getgid() });
    // SAFETY: as above.
    gids.push(unsafe { libc::getegid() });

    gids
}

#[cfg(target_os = "linux")]
/// Group membership *and* at least one readable keyboard, as one check: the
/// membership is the cause and the readable device is the effect, so reporting
/// them separately would only ever say the same thing twice.
fn check_input_access() -> CheckResult {
    let username = match current_username() {
        Ok(username) => username,
        Err(e) => return CheckResult::from_error(&e),
    };

    let content = std::fs::read_to_string(GROUP_FILE).unwrap_or_else(|e| {
        debug!("We could not read {GROUP_FILE}: {e}");
        String::new()
    });

    input_access_check(
        group_state(
            parse_input_group(&content).as_ref(),
            &username,
            &effective_gids(),
        ),
        &username,
        &probe_keyboard(),
    )
}

#[cfg(target_os = "linux")]
/// Renders check 3 from its two inputs, so every combination of them can be
/// exercised without a `/dev/input` to enumerate.
fn input_access_check(
    state: GroupState,
    username: &str,
    keyboard: &Result<String, crate::Error>,
) -> CheckResult {
    match state {
        GroupState::Missing => CheckResult::failed(
            format!(
                "There is no '{INPUT_GROUP}' group in {GROUP_FILE}, so nothing on this machine grants access to {INPUT_DIR} or {UINPUT_PATH}."
            ),
            SETUP_ADVICE,
        ),
        GroupState::NotConfigured => CheckResult::failed(
            format!(
                "Your user ('{username}') is not a member of the '{INPUT_GROUP}' group, so we cannot read the keyboard your listen hotkey lives on."
            ),
            SETUP_ADVICE,
        ),
        GroupState::Stale => CheckResult::failed(
            format!(
                "Your user ('{username}') is a member of the '{INPUT_GROUP}' group, but this session started before that was true, so the membership is not in effect yet."
            ),
            RELOGIN_ADVICE,
        ),
        // The group is in effect, so whatever discovery has to say about
        // /dev/input is the real state of things — including its errors, which
        // already carry the right advice for a permission problem.
        GroupState::Effective => match keyboard {
            Ok(name) => CheckResult::ok(format!(
                "You are in the '{INPUT_GROUP}' group, and we can read a keyboard to watch for your listen hotkey on ('{name}')."
            )),
            Err(e) => CheckResult::from_error(e),
        },
    }
}

#[cfg(target_os = "linux")]
/// Asks the hotkey discovery path, exactly as `run` would, whether there is a
/// keyboard under `/dev/input` we are allowed to read.
///
/// Going through [`discover_device`] rather than enumerating here is what makes
/// this check mean something: it is the same code, the same ranking and the
/// same errors a real run would hit. Left control stands in for "a hotkey",
/// because every keyboard has one and nothing else here does.
fn probe_keyboard() -> Result<String, crate::Error> {
    let key = keys::from_name(PROBE_KEY).expect("the probe key must be in the key table");

    discover_device(AUTO_DEVICE, key)
        .map(|device| device.name().unwrap_or("<unnamed device>").to_string())
}

// ── Check 4: an audio input ─────────────────────────────────────────────────

/// Whether the microphone we would actually listen on can be found.
///
/// `hint` is the resolved `audio.device` — the profile's, else the machine's,
/// else `default` — so this check answers the question a run would ask, not a
/// weaker one: a profile naming a microphone which is not plugged in fails here
/// rather than at the moment somebody starts a game.
fn check_audio_input(hint: &str) -> CheckResult {
    let host = cpal::default_host();

    let count = match host.input_devices() {
        Ok(devices) => devices.count(),
        Err(e) => {
            return CheckResult::failed(
                format!("We could not enumerate this machine's audio input devices ({e})."),
                &[
                    "Check that your sound server (PipeWire or PulseAudio) is running, and that the ALSA compatibility layer is installed.",
                ],
            );
        }
    };

    let total = format!(
        "{count} input {} in total",
        if count == 1 { "device" } else { "devices" }
    );

    match audio::select_input_device(&host, hint) {
        Ok(device) => CheckResult::ok(format!(
            "The microphone we would listen on is '{}' (from audio.device: {hint}; {total}).",
            device.name().unwrap_or_else(|_| "<unnamed device>".into())
        )),
        Err(e) => {
            let mut result = CheckResult::from_error(&e);
            result.advice.push(
                "Run 'voice-orders devices' to list every microphone this machine can see."
                    .to_string(),
            );
            result
        }
    }
}

// ── Check 5: the Vosk library ───────────────────────────────────────────────

/// Whether `libvosk.so` can be loaded.
///
/// This is the one check which used to be impossible to run: while the library
/// was a link-time dependency, a machine without it could not start
/// voice-orders at all, so there was nobody to report it. It is loaded on
/// demand now (`recognition/libvosk.rs`), which makes a missing library an
/// ordinary failing line with the install instructions attached.
fn check_libvosk() -> CheckResult {
    match libvosk::library_source() {
        Ok(source) => CheckResult::ok(format!(
            "The Vosk speech recognition library loaded from {source}."
        )),
        Err(e) => CheckResult::from_error(&e),
    }
}

// ── Check 6: a grammar-capable model ────────────────────────────────────────

/// Whether a model resolves, and whether it is one grammar mode can use.
///
/// A static-graph model is a *failure* rather than a warning: recognition is
/// constrained to the profile's phrases, so a model which cannot be constrained
/// defeats the design (DESIGN.md §"Model selection").
fn check_model(resolved: Result<PathBuf, crate::Error>) -> CheckResult {
    let path = match resolved {
        Ok(path) => path,
        Err(e) => return CheckResult::from_error(&e),
    };

    if !path.is_dir() {
        return CheckResult::failed(
            format!(
                "There is no speech model at '{}' — a Vosk model is a directory, unpacked from the archive you downloaded.",
                path.display()
            ),
            NO_MODEL_ADVICE,
        );
    }

    if path.join(DYNAMIC_GRAPH).exists() {
        return CheckResult::ok(format!(
            "The model at '{}' has a dynamic graph, so it can be constrained to your profile's phrases.",
            path.display()
        ));
    }

    CheckResult::failed(
        format!(
            "The model at '{}' has no '{DYNAMIC_GRAPH}', which means it was compiled with a static graph and cannot be constrained to your profile's phrases.",
            path.display()
        ),
        STATIC_GRAPH_ADVICE,
    )
}

// ── Check 7: the profile ────────────────────────────────────────────────────

/// Whether the profile named on the command line loads, whether its settings
/// resolve against this machine's configuration, and whether the device the
/// resulting hotkey lives on can be found.
///
/// The hotkey checked here is the *merged* one, so a profile which leaves the
/// block out and a machine which supplies it are diagnosed together — which is
/// the only way for the report to describe what a run would actually do.
fn check_profile(
    loaded: &Result<Profile, crate::Error>,
    settings: Option<&Result<ResolvedSettings, crate::Error>>,
) -> CheckResult {
    let profile = match loaded {
        Ok(profile) => profile,
        Err(e) => return CheckResult::from_error(e),
    };

    let name = profile.display_name();
    let hotkey = match settings {
        Some(Ok(settings)) => settings.hotkey.as_ref(),
        Some(Err(e)) => return CheckResult::from_error(e),
        None => None,
    };

    let Some(hotkey) = hotkey else {
        return CheckResult::ok(format!(
            "The profile '{name}' loads. It configures no hotkey, so voice-orders will listen continuously."
        ));
    };

    match resolve_hotkey_device(&hotkey.device, hotkey.key.code()) {
        Ok(device) => CheckResult::ok(format!(
            "The profile '{name}' loads, and its listen hotkey ({} in {} mode) resolves to '{}'.",
            hotkey.key, hotkey.mode, device
        )),
        Err(e) => CheckResult::from_error(&e),
    }
}

/// The name of the device a merged hotkey resolves to.
///
/// Goes through the same discovery a real run does, so the report cannot claim
/// a device `run` would not find.
#[cfg(target_os = "linux")]
fn resolve_hotkey_device(device: &str, key: KeyCode) -> Result<String, crate::Error> {
    discover_device(device, key)
        .map(|device| device.name().unwrap_or("<unnamed device>").to_string())
}

/// On Windows there is no per-device hotkey to resolve: the low-level keyboard
/// hook which replaces evdev watches every keyboard at once, so a configured
/// hotkey always resolves — and `hotkey.device`, which only ever meant a
/// `/dev/input/event*` node, is reported as the no-op it is here rather than
/// quietly ignored (DESIGN.md §"Windows support").
#[cfg(not(target_os = "linux"))]
fn resolve_hotkey_device(device: &str, _key: KeyCode) -> Result<String, crate::Error> {
    Ok(if device == "auto" {
        "every keyboard on this machine (a system-wide keyboard hook)".to_string()
    } else {
        format!(
            "every keyboard on this machine (a system-wide keyboard hook; 'hotkey.device: {device}' has no effect on Windows and can be left out)"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    // The table-driven tests are the `/etc/group` ones (Linux) and the
    // integrity-level one (Windows); each platform has some.
    use rstest::rstest;

    /// An `/etc/group` with the shape a real one has: other groups around the
    /// one we care about, and a trailing member list.
    #[cfg(target_os = "linux")]
    const GROUP_FILE_CONTENT: &str =
        "root:x:0:\naudio:x:995:alice\ninput:x:992:alice,bob\nwheel:x:998:alice\n";

    #[test]
    fn test_a_passing_check_renders_a_tick_and_no_advice() {
        let rendered =
            CheckResult::failed("something is wrong", &["fix it", "or fix it this way"]).render();
        assert_eq!(
            rendered,
            "✗ something is wrong\n    fix it\n    or fix it this way\n"
        );

        assert_eq!(CheckResult::ok("all good").render(), "✓ all good\n");
    }

    #[test]
    fn test_advice_is_only_shown_for_failures() {
        let mut passing = CheckResult::ok("all good");
        passing.advice.push("never printed".to_string());

        assert!(
            !passing.render().contains("never printed"),
            "a passing check has nothing to advise: {}",
            passing.render()
        );
    }

    #[test]
    fn test_a_report_counts_its_failures() {
        let rendered = render(&[
            CheckResult::ok("first"),
            CheckResult::failed("second", &["do something"]),
            CheckResult::ok("third"),
        ]);

        assert!(rendered.contains("✓ first\n"), "{rendered}");
        assert!(
            rendered.contains("✗ second\n    do something\n"),
            "{rendered}"
        );
        assert!(rendered.contains("3 checks — 1 failed.\n"), "{rendered}");
    }

    #[test]
    fn test_an_error_becomes_its_own_check() {
        let result = CheckResult::from_error(&human_errors::user(
            "We could not do the thing.",
            &["Try the other thing."],
        ));

        assert!(!result.ok);
        assert_eq!(result.headline, "We could not do the thing.");
        assert_eq!(result.advice, vec!["Try the other thing.".to_string()]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_a_missing_uinput_node_blames_the_module() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let result = check_uinput_node(&dir.path().join("uinput"));

        assert!(!result.ok);
        assert!(
            result
                .headline
                .contains("'uinput' kernel module is not loaded"),
            "unexpected headline: {}",
            result.headline
        );
        assert!(
            result
                .advice
                .iter()
                .any(|tip| tip.contains("voice-orders setup")),
            "the fix is a command we ship: {:?}",
            result.advice
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_an_existing_uinput_node_passes() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("uinput");
        std::fs::write(&path, "").expect("the file should be written");

        let result = check_uinput_node(&path);
        assert!(result.ok, "unexpected failure: {}", result.headline);
        assert!(result.headline.contains("exists"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_the_input_group_is_parsed_out_of_the_file() {
        let group = parse_input_group(GROUP_FILE_CONTENT).expect("the group should be found");

        assert_eq!(group.gid, 992);
        assert_eq!(group.members, vec!["alice".to_string(), "bob".to_string()]);
    }

    #[cfg(target_os = "linux")]
    #[rstest]
    #[case::no_input_group("root:x:0:\nwheel:x:998:alice\n")]
    #[case::empty("")]
    #[case::malformed("input:x:not-a-number:alice\n")]
    fn test_a_file_without_a_usable_input_group_parses_to_nothing(#[case] content: &str) {
        assert_eq!(parse_input_group(content), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_an_empty_member_list_is_not_a_member() {
        let group = parse_input_group("input:x:992:\n").expect("the group should be found");

        assert!(group.members.is_empty());
        assert!(!is_configured(Some(&group), "alice"));
    }

    #[rstest]
    // Nothing to be a member of.
    #[case::missing(None, "alice", &[992], GroupState::Missing)]
    // The group is there, but we are not in it.
    #[case::not_configured(Some((992, "bob")), "alice", &[992], GroupState::NotConfigured)]
    // We are in it, but this session predates that.
    #[case::stale(Some((992, "alice")), "alice", &[1000], GroupState::Stale)]
    // Everything is as it should be.
    #[cfg(target_os = "linux")]
    #[case::effective(Some((992, "alice")), "alice", &[1000, 992], GroupState::Effective)]
    fn test_group_state_distinguishes_configured_from_effective(
        #[case] group: Option<(u32, &str)>,
        #[case] username: &str,
        #[case] effective: &[u32],
        #[case] expected: GroupState,
    ) {
        let group = group.map(|(gid, member)| InputGroup {
            gid,
            members: vec![member.to_string()],
        });

        assert_eq!(group_state(group.as_ref(), username, effective), expected);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_a_missing_group_tells_you_to_run_setup() {
        let result = input_access_check(GroupState::Missing, "alice", &Ok(String::new()));

        assert!(!result.ok);
        assert!(
            result.headline.contains("no 'input' group"),
            "unexpected headline: {}",
            result.headline
        );
        assert!(
            result
                .advice
                .iter()
                .any(|tip| tip.contains("voice-orders setup")),
            "{:?}",
            result.advice
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_a_missing_membership_names_the_user_and_advises_setup() {
        let result = input_access_check(GroupState::NotConfigured, "alice", &Ok(String::new()));

        assert!(!result.ok);
        assert!(
            result.headline.contains("'alice'") && result.headline.contains("not a member"),
            "unexpected headline: {}",
            result.headline
        );
        assert!(
            result
                .advice
                .iter()
                .any(|tip| tip.contains("voice-orders setup")),
            "{:?}",
            result.advice
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_a_stale_membership_asks_for_a_re_login() {
        let result = input_access_check(GroupState::Stale, "alice", &Ok(String::new()));

        assert!(!result.ok);
        assert!(
            result.headline.contains("this session started before"),
            "the distinction people trip over must be spelled out: {}",
            result.headline
        );
        assert!(
            result
                .advice
                .iter()
                .any(|tip| tip.contains("Log out and back in")),
            "{:?}",
            result.advice
        );
        assert!(
            !result
                .advice
                .iter()
                .any(|tip| tip.contains("voice-orders setup")),
            "setup cannot fix a session which has already started: {:?}",
            result.advice
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_an_unreadable_input_device_folds_into_the_same_check() {
        // Discovery has already humanized this; the check must not paper over
        // it with a message of its own.
        let result = input_access_check(
            GroupState::Effective,
            "alice",
            &Err(human_errors::user(
                "We were not allowed to read /dev/input/event3.",
                &["Add yourself to the 'input' group."],
            )),
        );

        assert!(!result.ok);
        assert_eq!(
            result.headline,
            "We were not allowed to read /dev/input/event3."
        );
        assert_eq!(
            result.advice,
            vec!["Add yourself to the 'input' group.".to_string()]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_no_keyboard_is_a_failure_even_when_the_group_is_right() {
        let result = input_access_check(
            GroupState::Effective,
            "alice",
            &Err(human_errors::user(
                "We could not find any input device which reports the hotkey you configured (evdev code 29).",
                &["Run 'sudo evtest' and press your hotkey."],
            )),
        );

        assert!(!result.ok);
        assert!(
            result.headline.contains("could not find any input device"),
            "unexpected headline: {}",
            result.headline
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_a_readable_keyboard_passes_and_names_it() {
        let result = input_access_check(
            GroupState::Effective,
            "alice",
            &Ok("AT Translated Set 2 keyboard".to_string()),
        );

        assert!(result.ok, "unexpected failure: {}", result.headline);
        assert!(
            result.headline.contains("AT Translated Set 2 keyboard"),
            "unexpected headline: {}",
            result.headline
        );
    }

    #[test]
    fn test_the_report_says_where_the_system_configuration_came_from() {
        let loaded = SystemConfig {
            source: Some(PathBuf::from(
                "/home/alice/.config/voice-orders/config.yaml",
            )),
            ..SystemConfig::default()
        };

        assert_eq!(
            system_summary(&loaded),
            "System configuration: /home/alice/.config/voice-orders/config.yaml."
        );
    }

    #[test]
    fn test_the_report_says_where_it_looked_when_there_is_no_configuration() {
        let summary = system_summary(&SystemConfig::default());

        assert!(summary.contains("none at"), "{summary}");
        // The default path renders with the platform's separators.
        assert!(
            summary
                .replace('\\', "/")
                .contains("voice-orders/config.yaml"),
            "{summary}"
        );
        assert!(summary.contains("defaults apply"), "{summary}");
    }

    #[test]
    fn test_the_microphone_reported_is_the_merged_one() {
        let system: SystemConfig = serde_yaml::from_str("audio:\n  device: USB Microphone\n")
            .expect("the system configuration should load");

        // No profile: the machine's own default is what a run would use.
        assert_eq!(audio_device(None, &system), "USB Microphone");

        // A profile which names one wins.
        let profile = Profile::parse(&crate::config::LoadedProfile {
            source: "test-profile.yaml".to_string(),
            content: "audio:\n  device: Yeti\ngrammar: |\n  Salute = \"salute\" { x }\n"
                .to_string(),
        })
        .expect("the profile should load");
        let settings = ResolvedSettings::resolve(&profile, &system);

        assert_eq!(audio_device(Some(&settings), &system), "Yeti");
    }

    #[test]
    fn test_a_profile_whose_settings_do_not_resolve_fails_the_profile_check() {
        let profile = Profile::parse(&crate::config::LoadedProfile {
            source: "test-profile.yaml".to_string(),
            content:
                "name: Keyless\nhotkey:\n  device: auto\ngrammar: |\n  Salute = \"salute\" { x }\n"
                    .to_string(),
        })
        .expect("the profile should load — a missing key is a resolution problem, not a parse one");

        let settings = ResolvedSettings::resolve(&profile, &SystemConfig::default());
        let result = check_profile(&Ok(profile), Some(&settings));

        assert!(!result.ok);
        assert!(
            result.headline.contains("'key' field is missing"),
            "unexpected headline: {}",
            result.headline
        );
    }

    #[test]
    fn test_a_model_which_does_not_resolve_carries_its_own_advice() {
        let result = check_model(Err(human_errors::user(
            "We do not know which speech model to use.",
            &["Download one."],
        )));

        assert!(!result.ok);
        assert_eq!(result.headline, "We do not know which speech model to use.");
        assert_eq!(result.advice, vec!["Download one.".to_string()]);
    }

    #[test]
    fn test_a_model_which_is_not_there_says_so() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let result = check_model(Ok(dir.path().join("missing-model")));

        assert!(!result.ok);
        assert!(
            result.headline.contains("There is no speech model at"),
            "unexpected headline: {}",
            result.headline
        );
    }

    #[test]
    fn test_a_static_graph_model_fails_with_an_explanation() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::create_dir(dir.path().join("graph")).expect("the graph directory");

        let result = check_model(Ok(dir.path().to_path_buf()));

        assert!(!result.ok, "grammar mode is unavailable, which is fatal");
        assert!(
            result.headline.contains("static graph")
                && result.headline.contains("cannot be constrained"),
            "unexpected headline: {}",
            result.headline
        );
        assert!(
            result.advice.iter().any(|tip| tip.contains("lgraph")),
            "the advice should name a model which does work: {:?}",
            result.advice
        );
    }

    #[test]
    fn test_a_dynamic_graph_model_passes() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::create_dir(dir.path().join("graph")).expect("the graph directory");
        std::fs::write(dir.path().join(DYNAMIC_GRAPH), "").expect("the graph file");

        let result = check_model(Ok(dir.path().to_path_buf()));

        assert!(result.ok, "unexpected failure: {}", result.headline);
        assert!(
            result.headline.contains("dynamic graph"),
            "unexpected headline: {}",
            result.headline
        );
    }

    #[test]
    fn test_a_profile_which_does_not_load_is_reported_as_the_profile_check() {
        let result = check_profile(
            &Err(human_errors::user(
                "We could not read the profile at 'broken.yaml'.",
                &["Check the profile."],
            )),
            None,
        );

        assert!(!result.ok);
        assert!(result.headline.contains("broken.yaml"));
    }

    #[test]
    fn test_a_profile_without_a_hotkey_passes_without_touching_dev_input() {
        let profile = Profile::parse(&crate::config::LoadedProfile {
            source: "test-profile.yaml".to_string(),
            content: "name: Quiet\ngrammar: |\n  Salute = \"salute\" { x }\n".to_string(),
        })
        .expect("the profile should load");

        let settings = ResolvedSettings::resolve(&profile, &SystemConfig::default());
        let result = check_profile(&Ok(profile), Some(&settings));

        assert!(result.ok, "unexpected failure: {}", result.headline);
        assert!(
            result.headline.contains("listen continuously"),
            "unexpected headline: {}",
            result.headline
        );
    }

    #[test]
    fn test_without_a_profile_the_model_comes_from_the_command_line() {
        let resolved = resolve(
            Some(Path::new("/cli/model")),
            None,
            &SystemConfig::default(),
        )
        .expect("the override should resolve");
        assert_eq!(resolved, PathBuf::from("/cli/model"));
    }

    #[test]
    fn test_the_profile_model_is_used_when_a_profile_is_given() {
        let profile = Profile::parse(&crate::config::LoadedProfile {
            source: "test-profile.yaml".to_string(),
            content: "model: /profile/model\ngrammar: |\n  Salute = \"salute\" { x }\n".to_string(),
        })
        .expect("the profile should load");

        let resolved = resolve(None, Some(&profile), &SystemConfig::default())
            .expect("the profile should resolve");
        assert_eq!(resolved, PathBuf::from("/profile/model"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_the_username_comes_from_the_environment() {
        // Whichever of the two is set here, we must agree with it rather than
        // inventing a name.
        match (std::env::var_os("USER"), std::env::var_os("LOGNAME")) {
            (Some(user), _) if !user.is_empty() => {
                assert_eq!(
                    current_username().ok(),
                    Some(user.to_string_lossy().into_owned())
                );
            }
            (_, Some(logname)) if !logname.is_empty() => {
                assert_eq!(
                    current_username().ok(),
                    Some(logname.to_string_lossy().into_owned())
                );
            }
            _ => assert!(current_username().is_err()),
        }
    }

    #[cfg(target_os = "linux")]
    /// The real `/dev/uinput`, the real `/dev/input`, a real microphone and a
    /// real model: gated like every other hardware-touching test in the crate.
    #[tokio::test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    async fn real_machine_passes_the_hardware_checks() {
        for result in [
            check_uinput_node(Path::new(UINPUT_PATH)),
            check_virtual_keyboard().await,
            check_input_access(),
            check_audio_input("default"),
        ] {
            assert!(
                result.ok,
                "this machine should pass every hardware check, but: {}\n{}",
                result.headline,
                result.advice.join("\n")
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    fn real_input_devices_include_a_keyboard() {
        let name = probe_keyboard().expect("a readable keyboard should be discoverable");

        assert!(
            !name.is_empty(),
            "the discovered keyboard should have a name"
        );
    }

    #[test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    fn real_machine_has_libvosk() {
        let result = check_libvosk();

        assert!(
            result.ok,
            "the gated tests need libvosk, so this machine should have it: {}\n{}",
            result.headline,
            result.advice.join("\n")
        );
    }

    #[test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    fn real_model_has_a_dynamic_graph() {
        let path = std::env::var_os(MODEL_PATH_ENV).map_or_else(
            || {
                PathBuf::from(std::env::var_os("HOME").expect("HOME must be set"))
                    .join(".cache/vosk/vosk-model-small-en-us-0.15")
            },
            PathBuf::from,
        );

        if !path.is_dir() {
            eprintln!("skipping: no Vosk model at {}", path.display());
            return;
        }

        let result = check_model(Ok(path));
        assert!(
            result.ok,
            "the model on this machine should be grammar-capable: {}",
            result.headline
        );
    }

    // --- Check 3 (Windows): the integrity level -----------------------------

    #[cfg(not(target_os = "linux"))]
    #[rstest]
    // Elevated: nothing to warn about, so nothing is warned about.
    #[case(Some(true), false)]
    // The ordinary (and correct) case, which is also the one with the caveat.
    #[case(Some(false), true)]
    // Windows would not say; the caveat stands, because it might apply.
    #[case(None, true)]
    fn the_integrity_note_warns_only_when_elevated_windows_could_be_a_problem(
        #[case] elevated: Option<bool>,
        #[case] warns: bool,
    ) {
        let note = integrity_note(elevated);

        assert_eq!(
            note.contains("as administrator"),
            warns,
            "unexpected note: {note}"
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn the_integrity_level_never_fails_check_three() {
        // The report is informational: a hook which installed is a hook which
        // works, whatever this process' integrity level turns out to be.
        let result = check_keyboard_hook();

        assert!(result.ok, "unexpected failure: {}", result.headline);
        assert!(
            result.headline.ends_with(integrity_note(elevated())),
            "the integrity level belongs on the headline: {}",
            result.headline
        );
    }
}
