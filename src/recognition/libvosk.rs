//! Runtime loading of `libvosk.so`, and the slice of its C API we use.
//!
//! The obvious way to bind libvosk is `vosk-sys`, whose `extern` block carries
//! `#[link(name = "vosk")]`. That puts a `DT_NEEDED` entry in the executable,
//! which the dynamic loader resolves *before* `main` runs — so on a machine
//! without libvosk, voice-orders cannot start at all. Not `--version`, not
//! `setup`, and not `doctor`, which is precisely the command whose job it is to
//! explain what is missing. All the user gets is
//! `error while loading shared libraries: libvosk.so`.
//!
//! Binding the fifteen entry points ourselves and `dlopen`ing the library on
//! first use moves that failure into the program, where it becomes an ordinary
//! [`crate::Error`] carrying advice — and leaves every command which does not
//! recognize speech working normally.
//!
//! The library is opened once per process and never closed: [`API`] holds both
//! the handle and the function pointers into it, and lives for the life of the
//! program.

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    ptr::NonNull,
    sync::OnceLock,
};

use serde::Deserialize;
use tracing_batteries::prelude::*;

/// An environment variable naming `libvosk.so` directly, or the directory it
/// lives in. The escape hatch for a library which is installed somewhere the
/// dynamic loader does not look.
pub const LIB_PATH_ENV: &str = "VOSK_LIB_PATH";

/// The library's SONAME, which is what the loader is asked for when
/// [`LIB_PATH_ENV`] is not set.
const LIB_NAME: &str = "libvosk.so";

/// How to get libvosk onto this machine, offered whenever it cannot be loaded.
/// Literal rather than formatted because `human_errors` advice is `&'static`.
const MISSING_LIBRARY_ADVICE: &[&str] = &[
    "Download 'libvosk-linux-amd64.so' (or 'libvosk-linux-arm64.so' on ARM) from https://github.com/SierraSoftworks/voice-rs/releases/latest, then install it with 'sudo install -m 0644 libvosk-linux-amd64.so /usr/local/lib/libvosk.so && sudo ldconfig'.",
    "If you installed voice-orders with Homebrew, put that file at \"$(brew --prefix)/lib/libvosk.so\" instead; if you unpacked it yourself, a 'libvosk.so' next to the binary is enough.",
    "If libvosk is already installed somewhere else, set VOSK_LIB_PATH to it (or to the directory holding it).",
    "The installation guide at https://sierrasoftworks.github.io/voice-rs/guide/installation.html covers all of this in full.",
];

/// Kaldi's log verbosity, as libvosk's C API numbers it.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // the whole range is part of the binding; we only quiet it
pub enum LogLevel {
    /// Errors, warnings and info — libvosk's own default, and very chatty.
    Info = 0,
    /// Errors and warnings.
    Warn = -1,
    /// Errors only.
    Error = -2,
}

/// An opaque `VoskModel`, only ever handled behind a pointer.
#[repr(C)]
pub struct VoskModel {
    _opaque: [u8; 0],
}

/// An opaque `VoskRecognizer`, only ever handled behind a pointer.
#[repr(C)]
pub struct VoskRecognizer {
    _opaque: [u8; 0],
}

/// The entry points we resolve out of `libvosk.so`, plus the handle they point
/// into. Field names drop the `vosk_` prefix the C API carries.
struct Api {
    /// The `dlopen` handle. Never passed to `dlclose`: the function pointers
    /// below are addresses inside the library, and this struct is owned by a
    /// `static` which is never dropped.
    _handle: *mut c_void,

    /// What we asked `dlopen` for, so `doctor` can report where it came from.
    source: String,

    set_log_level: unsafe extern "C" fn(c_int),

    model_new: unsafe extern "C" fn(*const c_char) -> *mut VoskModel,
    model_free: unsafe extern "C" fn(*mut VoskModel),
    model_find_word: unsafe extern "C" fn(*mut VoskModel, *const c_char) -> c_int,

    recognizer_new_grm:
        unsafe extern "C" fn(*mut VoskModel, f32, *const c_char) -> *mut VoskRecognizer,
    recognizer_free: unsafe extern "C" fn(*mut VoskRecognizer),
    recognizer_set_max_alternatives: unsafe extern "C" fn(*mut VoskRecognizer, c_int),
    recognizer_set_words: unsafe extern "C" fn(*mut VoskRecognizer, c_int),
    recognizer_set_partial_words: unsafe extern "C" fn(*mut VoskRecognizer, c_int),
    recognizer_set_nlsml: unsafe extern "C" fn(*mut VoskRecognizer, c_int),
    recognizer_accept_waveform_s:
        unsafe extern "C" fn(*mut VoskRecognizer, *const i16, c_int) -> c_int,
    recognizer_result: unsafe extern "C" fn(*mut VoskRecognizer) -> *const c_char,
    recognizer_partial_result: unsafe extern "C" fn(*mut VoskRecognizer) -> *const c_char,
    recognizer_final_result: unsafe extern "C" fn(*mut VoskRecognizer) -> *const c_char,
    recognizer_reset: unsafe extern "C" fn(*mut VoskRecognizer),
}

// SAFETY: every field is an immutable address into a library which is never
// unloaded, so sharing `&Api` across threads hands out nothing but constants.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

/// The loaded library, or the reasons every candidate path failed. Resolved at
/// most once; a machine without libvosk does not pay for a `dlopen` per call.
static API: OnceLock<Result<Api, String>> = OnceLock::new();

/// The loaded library, or an actionable error explaining how to install it.
fn api() -> Result<&'static Api, crate::Error> {
    match API.get_or_init(load) {
        Ok(api) => Ok(api),
        Err(attempts) => Err(missing_library(attempts)),
    }
}

/// Where `libvosk.so` was loaded from, phrased for `doctor` to drop into a
/// sentence. Fails with the same error every other entry point does when the
/// library is not installed.
pub fn library_source() -> Result<String, crate::Error> {
    let api = api()?;

    // The bare SONAME means the loader found it for us, and naming the file
    // back at the user would say nothing about *where* it came from.
    Ok(if api.source == LIB_NAME {
        "the dynamic loader's search path".to_string()
    } else {
        format!("'{}'", api.source)
    })
}

/// Sets Kaldi's log verbosity, doing nothing when libvosk is unavailable —
/// the caller which actually needs the library reports that, and it should not
/// be reported twice.
pub fn set_log_level(level: LogLevel) {
    if let Ok(api) = api() {
        // SAFETY: the symbol was resolved from libvosk and takes an int.
        unsafe { (api.set_log_level)(level as c_int) }
    }
}

/// Opens the first candidate which loads, collecting each failure so that a
/// machine with no libvosk at all can be told everywhere we looked.
fn load() -> Result<Api, String> {
    let mut attempts = Vec::new();

    for candidate in candidates() {
        let name = candidate.to_string_lossy().into_owned();

        match unsafe { open(&candidate) } {
            Ok(handle) => {
                debug!(library = %name, "Loaded the Vosk speech recognition library.");
                // SAFETY: `handle` is a live library handle from `dlopen`.
                return unsafe { Api::resolve(handle, name) };
            }
            Err(e) => attempts.push(format!("'{name}' ({e})")),
        }
    }

    Err(attempts.join(", "))
}

/// The paths to try, in order: an explicit `$VOSK_LIB_PATH` (either the library
/// itself or the directory holding it), then the bare SONAME, which lets the
/// dynamic loader search the binary's `RUNPATH` — `$ORIGIN`, `$ORIGIN/../lib`
/// and the Homebrew prefix — followed by `$LD_LIBRARY_PATH`, the `ldconfig`
/// cache and the system directories.
fn candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(configured) = std::env::var_os(LIB_PATH_ENV) {
        let configured = PathBuf::from(configured);
        candidates.push(if configured.is_dir() {
            configured.join(LIB_NAME)
        } else {
            configured
        });
    }

    candidates.push(PathBuf::from(LIB_NAME));
    candidates
}

/// `dlopen`s a library, returning `dlerror`'s explanation on failure.
///
/// # Safety
///
/// Loading a library runs its initializers, so `path` must name a library
/// which is safe to bring into this process.
unsafe fn open(path: &std::path::Path) -> Result<*mut c_void, String> {
    let name = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| "the path contains a NUL byte".to_string())?;

    // SAFETY: `name` is a valid NUL-terminated string which outlives the call.
    // RTLD_LOCAL keeps libvosk's symbols out of the global namespace, and
    // RTLD_NOW resolves everything up front so a truncated or mismatched
    // library fails here rather than mid-utterance.
    let handle = unsafe {
        // `dlerror` is only meaningful immediately after a failed call, so the
        // previous error is cleared first.
        libc::dlerror();
        libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL)
    };

    if handle.is_null() {
        return Err(last_error().unwrap_or_else(|| "could not be loaded".to_string()));
    }

    Ok(handle)
}

/// `dlerror`, as an owned string.
fn last_error() -> Option<String> {
    // SAFETY: `dlerror` returns either NULL or a NUL-terminated string owned by
    // the loader, which we copy immediately.
    let message = unsafe { libc::dlerror() };
    if message.is_null() {
        return None;
    }

    // SAFETY: non-NULL `dlerror` output is a valid C string.
    Some(
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned(),
    )
}

impl Api {
    /// Resolves every symbol we need out of an open library.
    ///
    /// # Safety
    ///
    /// `handle` must be a live handle returned by `dlopen`, and the library it
    /// names must be libvosk — the signatures below are asserted, not checked.
    // Each `symbol!` result is assigned straight into a field whose type spells
    // the signature out, so annotating the transmute again at every call site
    // would only add a second place for the two to disagree.
    #[allow(clippy::missing_transmute_annotations)]
    unsafe fn resolve(handle: *mut c_void, source: String) -> Result<Self, String> {
        /// Resolves one symbol, transmuting it to the declared signature.
        macro_rules! symbol {
            ($name:literal) => {{
                // SAFETY: the caller guarantees a live handle; the symbol name
                // is a NUL-terminated literal.
                let address = unsafe {
                    libc::dlerror();
                    libc::dlsym(handle, concat!($name, "\0").as_ptr().cast())
                };

                if address.is_null() {
                    return Err(format!(
                        "'{source}' does not export {}{}",
                        $name,
                        last_error().map(|e| format!(" ({e})")).unwrap_or_default()
                    ));
                }

                // SAFETY: libvosk exports this symbol as a function with the
                // signature the field it is assigned to declares.
                unsafe { std::mem::transmute(address) }
            }};
        }

        Ok(Self {
            _handle: handle,
            set_log_level: symbol!("vosk_set_log_level"),
            model_new: symbol!("vosk_model_new"),
            model_free: symbol!("vosk_model_free"),
            model_find_word: symbol!("vosk_model_find_word"),
            recognizer_new_grm: symbol!("vosk_recognizer_new_grm"),
            recognizer_free: symbol!("vosk_recognizer_free"),
            recognizer_set_max_alternatives: symbol!("vosk_recognizer_set_max_alternatives"),
            recognizer_set_words: symbol!("vosk_recognizer_set_words"),
            recognizer_set_partial_words: symbol!("vosk_recognizer_set_partial_words"),
            recognizer_set_nlsml: symbol!("vosk_recognizer_set_nlsml"),
            recognizer_accept_waveform_s: symbol!("vosk_recognizer_accept_waveform_s"),
            recognizer_result: symbol!("vosk_recognizer_result"),
            recognizer_partial_result: symbol!("vosk_recognizer_partial_result"),
            recognizer_final_result: symbol!("vosk_recognizer_final_result"),
            recognizer_reset: symbol!("vosk_recognizer_reset"),
            // Last, because every `symbol!` above borrows it for its error.
            source,
        })
    }
}

/// The error every entry point reports when the library cannot be loaded: what
/// we tried, and the shortest route to a working install.
fn missing_library(attempts: &str) -> crate::Error {
    human_errors::user(
        format!(
            "We could not load the Vosk speech recognition library ({LIB_NAME}), which voice-orders needs in order to understand speech. We tried {attempts}."
        ),
        MISSING_LIBRARY_ADVICE,
    )
}

/// A loaded speech model.
///
/// Freed on drop; libvosk reference-counts the underlying model, so a
/// [`Recognizer`] built from it stays valid until it is itself freed.
pub struct Model {
    handle: NonNull<VoskModel>,
    api: &'static Api,
}

// SAFETY: libvosk's model is internally synchronized and reference counted, so
// ownership can move between threads — which is what puts the decoder on the
// recognizer thread. Mirrors the `vosk` crate's own impls.
unsafe impl Send for Model {}
unsafe impl Sync for Model {}

impl Model {
    /// Loads the model at `path`.
    ///
    /// `Err` means libvosk itself is unavailable; `Ok(None)` means the library
    /// loaded but refused the model, which the caller reports in terms of the
    /// model rather than the library.
    pub fn open(path: &str) -> Result<Option<Self>, crate::Error> {
        let api = api()?;
        let path = CString::new(path).map_err(|_| {
            human_errors::user(
                format!("The model path '{path}' contains a NUL byte, which Vosk cannot accept."),
                &["Move the model to a directory whose path contains no NUL bytes."],
            )
        })?;

        // SAFETY: `path` is a valid C string which outlives the call.
        let handle = unsafe { (api.model_new)(path.as_ptr()) };

        Ok(NonNull::new(handle).map(|handle| Self { handle, api }))
    }

    /// The model's symbol for `word`, or [`None`] when it cannot recognize it.
    pub fn find_word(&self, word: &str) -> Option<u32> {
        let word = CString::new(word).ok()?;

        // SAFETY: our handle is live and `word` is a valid C string.
        let symbol = unsafe { (self.api.model_find_word)(self.handle.as_ptr(), word.as_ptr()) };

        u32::try_from(symbol).ok()
    }
}

impl Drop for Model {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by `vosk_model_new` and is freed once.
        unsafe { (self.api.model_free)(self.handle.as_ptr()) }
    }
}

/// A grammar-constrained recognizer, decoding audio against one model.
pub struct Recognizer {
    handle: NonNull<VoskRecognizer>,
    api: &'static Api,
}

// SAFETY: as for [`Model`] — the recognizer is moved onto the decoder thread
// and only ever touched from there. Mirrors the `vosk` crate's own impls.
unsafe impl Send for Recognizer {}
unsafe impl Sync for Recognizer {}

impl Recognizer {
    /// Builds a recognizer constrained to `phrases`, returning [`None`] when
    /// the model cannot be constrained to a grammar (a static-graph model) or
    /// the sample rate is one it will not decode.
    pub fn with_grammar(model: &Model, sample_rate: f32, phrases: &[String]) -> Option<Self> {
        let grammar = CString::new(serde_json::to_string(phrases).ok()?).ok()?;

        // SAFETY: the model handle is live for the duration of the call and
        // `grammar` is a valid C string; libvosk copies the grammar it is given.
        let handle = unsafe {
            (model.api.recognizer_new_grm)(model.handle.as_ptr(), sample_rate, grammar.as_ptr())
        };

        Some(Self {
            handle: NonNull::new(handle)?,
            api: model.api,
        })
    }

    /// How many alternative transcripts to return; `0` is a single best result.
    pub fn set_max_alternatives(&mut self, count: i32) {
        // SAFETY: our handle is live.
        unsafe { (self.api.recognizer_set_max_alternatives)(self.handle.as_ptr(), count) }
    }

    /// Whether finalized results carry per-word metadata.
    pub fn set_words(&mut self, enabled: bool) {
        // SAFETY: our handle is live.
        unsafe { (self.api.recognizer_set_words)(self.handle.as_ptr(), c_int::from(enabled)) }
    }

    /// Whether partial results carry per-word metadata.
    pub fn set_partial_words(&mut self, enabled: bool) {
        // SAFETY: our handle is live.
        unsafe {
            (self.api.recognizer_set_partial_words)(self.handle.as_ptr(), c_int::from(enabled))
        }
    }

    /// Whether results are formatted as NLSML rather than JSON.
    pub fn set_nlsml(&mut self, enabled: bool) {
        // SAFETY: our handle is live.
        unsafe { (self.api.recognizer_set_nlsml)(self.handle.as_ptr(), c_int::from(enabled)) }
    }

    /// Feeds a frame of 16-bit mono PCM to the decoder.
    pub fn accept_waveform(&mut self, data: &[i16]) -> Result<DecodingState, BufferTooLong> {
        let length = c_int::try_from(data.len()).map_err(|_| BufferTooLong(data.len()))?;

        // SAFETY: our handle is live and `data` is valid for `length` samples.
        let state = unsafe {
            (self.api.recognizer_accept_waveform_s)(self.handle.as_ptr(), data.as_ptr(), length)
        };

        Ok(DecodingState::from_c_int(state))
    }

    /// The transcript of the utterance the endpointer has just finalized.
    pub fn result(&mut self) -> String {
        // SAFETY: our handle is live; the returned buffer is owned by libvosk
        // and stays valid until the next call on this recognizer.
        transcript(unsafe { (self.api.recognizer_result)(self.handle.as_ptr()) })
    }

    /// The in-progress hypothesis.
    pub fn partial_result(&mut self) -> String {
        // SAFETY: as for `result`.
        transcript(unsafe { (self.api.recognizer_partial_result)(self.handle.as_ptr()) })
    }

    /// Flushes the decoder and returns whatever it was still holding.
    pub fn final_result(&mut self) -> String {
        // SAFETY: as for `result`.
        transcript(unsafe { (self.api.recognizer_final_result)(self.handle.as_ptr()) })
    }

    /// Drops the current utterance and starts decoding afresh.
    pub fn reset(&mut self) {
        // SAFETY: our handle is live.
        unsafe { (self.api.recognizer_reset)(self.handle.as_ptr()) }
    }
}

impl Drop for Recognizer {
    fn drop(&mut self) {
        // SAFETY: the handle came from `vosk_recognizer_new_grm` and is freed
        // once. The model outlives it: `Session` declares them in that order.
        unsafe { (self.api.recognizer_free)(self.handle.as_ptr()) }
    }
}

/// What `accept_waveform` made of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodingState {
    /// The endpointer fired: an utterance is waiting in `result`.
    Finalized,
    /// Decoding continues.
    Running,
    /// The decoder rejected the frame.
    Failed,
}

impl DecodingState {
    /// The variant `vosk_recognizer_accept_waveform_s`'s return value denotes.
    fn from_c_int(value: c_int) -> Self {
        match value {
            1 => Self::Finalized,
            0 => Self::Running,
            _ => Self::Failed,
        }
    }
}

/// A frame longer than the C API's `int` length parameter can describe. Our
/// frames are 100 ms, so this is a guard rather than a reachable state.
#[derive(Debug)]
pub struct BufferTooLong(pub usize);

impl std::fmt::Display for BufferTooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the audio buffer held {} samples (expected fewer than {})",
            self.0,
            c_int::MAX
        )
    }
}

impl std::error::Error for BufferTooLong {}

/// libvosk's result JSON, in both the shapes it can take: a single transcript
/// by default, and an alternatives list when `max_alternatives` is non-zero.
/// Partial results use the same envelope with a `partial` field.
#[derive(Debug, Default, Deserialize)]
struct ResultJson {
    #[serde(default)]
    text: String,
    #[serde(default)]
    partial: String,
    #[serde(default)]
    alternatives: Vec<Alternative>,
}

/// One entry of an n-best list.
#[derive(Debug, Deserialize)]
struct Alternative {
    #[serde(default)]
    text: String,
}

/// Reads the transcript out of one of libvosk's JSON results, tolerating the
/// multi-alternative shape even though we keep `max_alternatives` at 0.
///
/// # Safety
///
/// Only called with a pointer libvosk has just returned, which is either NULL
/// or a NUL-terminated buffer it owns.
fn transcript(raw: *const c_char) -> String {
    if raw.is_null() {
        return String::new();
    }

    // SAFETY: non-NULL results from libvosk are valid C strings, owned by the
    // recognizer and valid until its next call — we copy before returning.
    let json = unsafe { CStr::from_ptr(raw) }.to_string_lossy();

    let parsed: ResultJson = match serde_json::from_str(&json) {
        Ok(parsed) => parsed,
        Err(e) => {
            warn!(error = %e, "The speech recognizer returned a result we could not read.");
            return String::new();
        }
    };

    if !parsed.text.is_empty() {
        return parsed.text;
    }
    if !parsed.partial.is_empty() {
        return parsed.partial;
    }

    parsed
        .alternatives
        .into_iter()
        .next()
        .map(|alternative| alternative.text)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn transcript_reads_a_finalized_result() {
        let json = CString::new(r#"{"text": "deploy the sentry"}"#).unwrap();

        assert_eq!(transcript(json.as_ptr()), "deploy the sentry");
    }

    #[test]
    fn transcript_reads_a_partial_result() {
        let json = CString::new(r#"{"partial": "deploy the"}"#).unwrap();

        assert_eq!(transcript(json.as_ptr()), "deploy the");
    }

    #[test]
    fn transcript_reads_the_first_alternative() {
        let json = CString::new(
            r#"{"alternatives": [{"text": "deploy the sentry"}, {"text": "deploy this entry"}]}"#,
        )
        .unwrap();

        assert_eq!(transcript(json.as_ptr()), "deploy the sentry");
    }

    #[test]
    fn transcript_is_empty_for_silence_and_for_nonsense() {
        let empty = CString::new(r#"{"text": ""}"#).unwrap();
        assert_eq!(transcript(empty.as_ptr()), "");

        let nonsense = CString::new("not json at all").unwrap();
        assert_eq!(transcript(nonsense.as_ptr()), "");

        assert_eq!(transcript(std::ptr::null()), "");
    }

    #[test]
    fn decoding_state_maps_the_c_return_values() {
        assert_eq!(DecodingState::from_c_int(1), DecodingState::Finalized);
        assert_eq!(DecodingState::from_c_int(0), DecodingState::Running);
        assert_eq!(DecodingState::from_c_int(-1), DecodingState::Failed);
    }

    #[test]
    fn candidates_prefer_the_configured_path() {
        // The env var is process-global, so this test owns it for its duration
        // and puts it back; `cargo test` runs tests in threads.
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(
            with_lib_path(Some(dir.path().as_os_str()), candidates),
            vec![dir.path().join(LIB_NAME), PathBuf::from(LIB_NAME)],
            "a directory should be joined with the library's name"
        );

        let file = dir.path().join("vosk-custom.so");
        assert_eq!(
            with_lib_path(Some(file.as_os_str()), candidates),
            vec![file, PathBuf::from(LIB_NAME)],
            "a path which is not a directory should be used as-is"
        );

        assert_eq!(
            with_lib_path(None, candidates),
            vec![PathBuf::from(LIB_NAME)]
        );
    }

    /// Runs `body` with [`LIB_PATH_ENV`] set to `value`, restoring it after.
    fn with_lib_path<T>(value: Option<&OsStr>, body: fn() -> T) -> T {
        static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = GUARD.lock().unwrap_or_else(|e| e.into_inner());

        let previous = std::env::var_os(LIB_PATH_ENV);
        // SAFETY: the mutex above serializes every mutation of this variable in
        // this test binary, and nothing else in the crate writes it.
        unsafe {
            match value {
                Some(value) => std::env::set_var(LIB_PATH_ENV, value),
                None => std::env::remove_var(LIB_PATH_ENV),
            }
        }

        let result = body();

        // SAFETY: as above.
        unsafe {
            match previous {
                Some(previous) => std::env::set_var(LIB_PATH_ENV, previous),
                None => std::env::remove_var(LIB_PATH_ENV),
            }
        }

        result
    }

    #[test]
    fn missing_library_advises_how_to_install_it() {
        let error = missing_library("'libvosk.so' (not found)");
        let rendered = human_errors::pretty(&error).to_string();

        assert!(
            rendered.contains("libvosk-linux-amd64.so"),
            "the advice should name the release asset: {rendered}"
        );
        assert!(
            rendered.contains(LIB_PATH_ENV),
            "the advice should mention the override: {rendered}"
        );
    }
}
