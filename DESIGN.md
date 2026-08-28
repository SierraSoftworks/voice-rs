# voice-orders — Design

`voice-orders` is a Linux-native voice macro tool in the spirit of VoiceAttack and LinVAM: you speak a
command phrase, it presses keys in a game (or any application) for you. It is built around three ideas:

1. **Grammar-constrained recognition.** Rather than transcribing free speech and hoping for the best,
   every profile is compiled into an optimized [Vosk](https://alphacephei.com/vosk/) recognition
   grammar containing only the phrases the profile can actually act on. This dramatically improves
   recognition accuracy for the domain vocabulary (and lets us validate profiles against the model's
   vocabulary ahead of time).
2. **Kernel-level input.** Hotkeys are read from `/dev/input/event*` (evdev) and output is emitted
   through `/dev/uinput` as a virtual keyboard. Both sit below the display server, so the tool works
   identically on X11 and Wayland — and, crucially, inside fullscreen games.
3. **A launch-wrapper workflow.** `voice-orders run profile.yaml -- <app> <args...>` starts the app as
   a child process and exits when it exits, which makes it a drop-in Steam launch option:
   `voice-orders run drg.yaml -- %command%`.

## Goals & non-goals

**v1 goals**

- Vosk-based recognition using a compiled per-profile grammar (never transcription mode).
- A small phrase DSL supporting required words, `[optional]` words, `{alternate, choices}`, and
  arbitrary nesting of the two.
- Ambiguous-prefix commands ("autocannon" vs "autocannon sentry") resolved by a configurable
  statement-completion timeout.
- Non-blocking detection path on Tokio: audio capture, recognition, matching, and key output are
  separate tasks/threads connected by bounded channels, with recognized commands flowing through a
  command queue.
- A global listen hotkey with three modes: toggle, push-to-talk, push-to-mute.
- Keyboard output only, expressed either as explicit events (down/up/wait) or as key-press sequences
  with a shared duration/interval.
- Profiles as standalone YAML files, loadable from local paths or `https://` URLs (including GitHub
  Gists) for easy sharing.
- `clap` CLI with `new`, `validate`, and `run` subcommands. `validate` checks every grammar word
  against the model vocabulary and suggests alternatives or decompositions for unknown words.

**Explicit non-goals for v1** (future work, but the design leaves room for them)

- Mouse movement/click output, process execution, and audio playback outputs — `CompiledOutput` is an
  enum precisely so these can be added without touching the pipeline.
- Runtime grammar switching (per-app sub-profiles).
- Partial-result ("eager") command firing — see [Endpointing](#endpointing-and-latency).
- Windows/macOS support. This tool is deliberately Linux-first at the evdev/uinput layer.

## Architectural lineage

This project deliberately borrows from three sibling SierraSoftworks projects:

- **[github-backup](https://github.com/SierraSoftworks/github-backup)** — overall crate shape: a
  single binary crate with a flat module tree, `human-errors` as the *only* error type
  (`pub use human_errors::Error` at the crate root so `crate::Error` works everywhere), a thin
  `main()` that sets up `tracing-batteries` telemetry and renders failures with
  `human_errors::pretty`, config loading through serde with advice-carrying wrap errors, a
  `HumanizableError` trait for foreign error types, the `version!()` macro, and the CI/release
  pipeline (fmt + clippy `-D warnings`, grcov coverage, release-drafter, nightly audit, matrix
  release builds with the version `sed`-ed in from the git tag).
- **[filt-rs](https://github.com/SierraSoftworks/filters)** — the parser architecture: a zero-copy
  `Scanner<'a>` that *is* the token iterator (errors are stream items), `Loc { line, column }`
  computed on demand with no span table, a recursive-descent parser with a `nested()` depth guard,
  errors built as `human_errors::user(format!("… at {loc} …"), &[static advice])`, an AST that
  borrows `&'a str` from the source, an owner type holding a `Pin<Box<String>>` alongside a
  `'static` AST so parsed grammars can live inside config structs, and parse-during-deserialize so a
  bad phrase is a config-load error rather than a runtime one.
- **[grey](https://github.com/SierraSoftworks/grey)** — the documentation website (VuePress 2 under
  `docs/`, deployed with the GitHub Pages artifact actions) and the validation philosophy: let the
  type system and serde validate everything they can at load time, keep explicit `validate_*()`
  methods for cross-field invariants with messages that name the offending entity, and keep example
  YAML files honest by loading them in unit tests.

## Crate & module layout

Single binary crate `voice-orders`, edition 2024:

```
voice-orders/
├── Cargo.toml
├── DESIGN.md                      # this document
├── examples/
│   └── profile.yaml               # canonical example, exercised by a unit test
├── docs/                          # VuePress 2 site (see Documentation)
├── .github/workflows/             # rust.yml, docs-website.yml, release-drafter, audit
└── src/
    ├── main.rs                    # Args (clap derive), signal → CancellationToken bridge,
    │                              # Session::new("voice-orders", version!()), dispatch,
    │                              # human_errors::pretty on failure + exit(1)
    ├── macros.rs                  # version!(): "0.0.0-dev" in debug, CARGO_PKG_VERSION in release
    ├── errors.rs                  # HumanizableError impls (reqwest, io, cpal, vosk, uinput)
    ├── telemetry.rs               # tracing-batteries session helpers
    ├── commands/
    │   ├── mod.rs                 # Command enum (clap Subcommand) + dispatch
    │   ├── new.rs                 # scaffold a commented profile
    │   ├── validate.rs            # structure + vocabulary + lints
    │   └── run.rs                 # pipeline assembly + child process supervision
    ├── config/
    │   ├── mod.rs                 # Profile (serde), cross-field validate_*() methods
    │   ├── command.rs             # CommandConfig: phrase + output (both YAML forms)
    │   ├── hotkey.rs              # HotkeyConfig { device, key, mode }
    │   ├── output.rs              # OutputConfig → compiled KeyEvent plans
    │   ├── duration.rs            # humantime deserialize_with helpers
    │   └── loader.rs              # path-vs-https detection, reqwest fetch, gist raw rewrite
    ├── grammar/
    │   ├── mod.rs                 # CommandPhrase owner type (Pin<Box<String>> + 'static AST)
    │   ├── location.rs            # Loc { line, column } + Display "line X, column Y"
    │   ├── token.rs               # Token<'a> variants, all carrying Loc
    │   ├── lexer.rs               # Scanner<'a>: Iterator<Item = Result<Token<'a>, Error>>
    │   ├── parser.rs              # recursive descent, Peekable<I>, nested() depth guard
    │   ├── expr.rs                # Node<'a> / PhraseExpr<'a> AST + round-trip Display
    │   └── expansion.rs           # AST → deduped concrete phrase list + linear word set
    ├── audio/
    │   └── mod.rs                 # cpal capture: device selection, format conversion, frame channel
    ├── recognition/
    │   ├── mod.rs                 # Recognition/Vocabulary traits (mockable), RecognitionEvent
    │   └── vosk.rs                # Vosk Model/Recognizer wrapper + dedicated thread loop
    ├── matcher/
    │   ├── mod.rs                 # matcher task: event loop + completion-timeout state machine
    │   └── trie.rs                # word trie phrase table
    ├── hotkey/
    │   └── mod.rs                 # evdev device discovery, EventStream task, ListenMode logic
    └── output/
        ├── mod.rs                 # executor task consuming the command queue
        ├── keys.rs                # KeyCode + name table (macro-generated, single source of truth)
        └── uinput.rs              # uinput-tokio device wrapper, release-on-shutdown safety
```

### Dependencies

```toml
[dependencies]
clap = { version = "4", features = ["derive"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time", "process", "signal", "fs"] }
tokio-util = "0.7"                    # CancellationToken
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
human-errors = { version = "0.2", features = ["pretty"] }
humantime = "2"
serde_json = "1"                      # libvosk's result envelopes
cpal = "0.16"
evdev = { version = "0.13", features = ["tokio"] }
uinput-tokio = "0.1"
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls"] }
tracing-batteries = { git = "https://github.com/sierrasoftworks/tracing-batteries-rs", features = ["opentelemetry"] }
strsim = "0.11"                       # did-you-mean suggestions

[dev-dependencies]
rstest = "0.23"
wiremock = "0.6"
tempfile = "3"

[features]
default = []
pure_tests = []                       # gates tests requiring a Vosk model / audio hardware
```

Verified API facts this design relies on (`vosk` 0.3.1):

- `Recognizer::new_with_grammar(model: &Model, sample_rate: f32, grammar: &[impl AsRef<str>]) -> Option<Self>`
  — grammar-constrained recognition.
- `Recognizer::accept_waveform(&mut self, data: &[i16]) -> Result<DecodingState, _>` — PCM 16-bit
  mono; on `DecodingState::Finalized` read `result()`, otherwise `partial_result()`.
- `Model::find_word(&mut self, word) -> Option<u32>` — vocabulary membership, the backbone of
  `validate`.
- `evdev` 0.13 ships a Tokio `EventStream` behind the `tokio` feature; `uinput-tokio` 0.1.x provides
  the async virtual-device builder and key events.

## Runtime pipeline

```
                 ┌───────────────────────────────────────────────────────────────┐
                 │                         tokio runtime                         │
                 │                                                               │
 /dev/input/evX ─► hotkey task (evdev EventStream) ──► watch::Sender<bool>       │
                 │      (toggle / PTT / PTM logic)        "listening"            │
                 │                                     │            │            │
                 │                        AtomicBool mirror     on mute:         │
                 │                        (read by callback)    Reset + Muted    │
                 │                                     │            │            │
 cpal callback   │                                     ▼            ▼            │
 (RT thread) ────► bounded sync_channel(8) ─────► recognizer thread (std)       │
   any-rate in      AudioMsg::{Frame(Vec<i16>),    owns Model + Recognizer,     │
   16k mono out                 Reset}             accept_waveform loop         │
                    try_send, drop-newest,             │                        │
                    dropped-frame counter              ▼                        │
                 │              mpsc<RecognitionEvent> (cap 64)                 │
                 │                 Partial / Final / Muted                      │
                 │                             │                                │
                 │                             ▼                                │
                 │      matcher task: trie walk + completion-timeout            │
                 │      state machine (select! on events / sleep_until)         │
                 │                             │                                │
                 │              mpsc<CommandAction> (cap 32)  ← the command queue
                 │                             │                                │
                 │                             ▼                                │
                 │      executor task ──► uinput virtual keyboard (/dev/uinput) │
                 │                                                              │
                 │      child supervisor: tokio::process wait()                 │
                 │        └─ child exit / SIGINT / SIGTERM ─► CancellationToken │
                 └───────────────────────────────────────────────────────────────┘
```

### Audio capture (cpal)

cpal's ALSA host works on every modern Linux system (PipeWire ships `pipewire-alsa` compatibility
everywhere), needs no PipeWire-specific code, and is the best-trodden path for feeding Vosk. We
request a 16 kHz mono i16 input config matching the model's expected rate; when the device refuses
(PipeWire defaults are typically 48 kHz f32), we take the nearest supported config and convert inside
the callback: channel-average to mono, then decimate to the model rate (48 kHz → 16 kHz is an exact
take-every-3rd with a cheap averaging filter). Speech recognition is tolerant of naive decimation; if
accuracy suffers in practice, swapping in `rubato` is an isolated change inside `audio/`.

The cpal callback runs on a realtime thread and must never block. It pushes ~100 ms frames into a
bounded `std::sync::mpsc::sync_channel(8)` (≈800 ms of buffer) using `try_send`; when the channel is
full the *new* frame is dropped and an `AtomicU64` dropped-frame counter is incremented, which the
recognizer thread logs periodically as a warning — dropped audio mid-utterance should be surfaced,
not silently smoothed over. While listening is off, the callback reads an `AtomicBool` mirror of the
listening state and drops frames at the source, so a muted microphone costs nothing downstream.

### Recognition (dedicated thread)

`accept_waveform` is continuously CPU-bound for the lifetime of the process, which is exactly what
`spawn_blocking` is *not* for (it would permanently pin a blocking-pool slot and make ownership of
the stateful `Recognizer` awkward). Instead a dedicated named `std::thread` owns the `Model` and the
grammar-constrained `Recognizer`, blocks on the audio channel, and pushes events into the Tokio side:

```rust
// recognition/mod.rs
pub enum RecognitionEvent {
    Partial(String),   // unstable, may be revised; emitted only when the text changes
    Final(String),     // utterance finalized by Vosk's endpointer
    Muted,             // listening turned off; matcher must clear all state
}

pub enum AudioMsg {
    Frame(Vec<i16>),
    Reset,             // listening turned off; recognizer calls Recognizer::reset()
}

/// Object-safe seam so matcher/executor/validate tests never touch libvosk.
pub trait Vocabulary {
    fn contains(&mut self, word: &str) -> bool;        // Model::find_word(word).is_some()
    fn words(&self) -> Option<Vec<String>>;            // model/graph/words.txt when readable
}

// recognition/vosk.rs
pub fn spawn_recognizer(
    model_path: &Path,
    sample_rate: u32,
    grammar: &[String],                                // expanded phrases + "[unk]"
    events: tokio::sync::mpsc::Sender<RecognitionEvent>,
) -> Result<(RecognizerHandle, std::sync::mpsc::SyncSender<AudioMsg>), crate::Error>;
```

Shutdown is naturally tied to channel closure: when the audio sender is dropped the loop ends and the
thread joins. `AudioMsg::Reset` travels in-band on the same channel so mute ordering is exact — a
half-spoken phrase can never leak across a mute boundary.

### Cancellation

github-backup uses a `static CANCEL: AtomicBool` because it has one sequential work loop.
voice-orders has five concurrent tasks that all need to *wake up* on shutdown, which is exactly what
`tokio_util::sync::CancellationToken` provides (`token.cancelled()` composes into every task's
`select!`). The SIGINT/SIGTERM handlers (via `tokio::signal`) cancel the token; the two non-Tokio
threads (cpal callback, recognizer) exit via channel closure instead, so no atomics are needed for
shutdown. `main()` stays thin per house style.

## Grammar DSL

Command phrases are written in a small DSL:

```
deploy [the] {autocannon, auto cannon} [sentry]
```

- plain words are required, in order;
- `[optional]` groups may be omitted;
- `{alternate, choices}` groups require exactly one branch;
- the two nest freely: `[{optional, elective}] combinations`.

### EBNF

```
phrase     = term , { term } ;
term       = word | optional | alternates ;
optional   = "[" , phrase , "]" ;
alternates = "{" , phrase , { "," , phrase } , "}" ;
word       = word-char , { word-char } ;
word-char  = letter | digit | "'" | "-" ;
```

Whitespace separates words and is otherwise insignificant. Nesting is capped at
`MAX_NESTING_DEPTH = 8` — deeper phrases are almost certainly a mistake, and the cap bounds parser
recursion.

### Lexer and parser (filt-rs style)

The lexer is a zero-copy scanner over the source string, and it *is* the token stream — errors are
items, so a lex error surfaces at exactly the point the parser demands the bad token:

```rust
// grammar/token.rs
#[derive(Debug, PartialEq)]
pub enum Token<'a> {
    Word(Loc, &'a str),
    LeftBracket(Loc), RightBracket(Loc),
    LeftBrace(Loc), RightBrace(Loc),
    Comma(Loc),
}

// grammar/lexer.rs
pub struct Scanner<'a> {
    source: &'a str,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    line: usize,
    line_start: usize,     // byte offset of the current line; column = 1 + idx - line_start
}

impl<'a> Iterator for Scanner<'a> {
    type Item = Result<Token<'a>, crate::Error>;
    // ...
}
```

`Loc { line, column }` is computed on demand — no span table. Every token variant carries its `Loc`;
word payloads are `&'a str` slices of the source (no allocation in the lexer).

The parser is recursive descent over `Peekable<I>` where `I: Iterator<Item = Result<Token<'a>, Error>>`
(generic so tests can drive it directly), with one method per production and a `nested()` wrapper
guarding the two recursion points (`[`, `{`) against depth abuse:

```rust
// grammar/expr.rs
#[derive(Debug, PartialEq, Clone)]
pub enum Node<'a> {
    Word(Loc, &'a str),
    Optional(Loc, Vec<Node<'a>>),          // [ ... ] — the whole sequence is optional
    Alternates(Loc, Vec<Vec<Node<'a>>>),   // { seq, seq, ... }
}

#[derive(Debug, PartialEq, Clone)]
pub struct PhraseExpr<'a>(pub Vec<Node<'a>>);
```

The AST borrows from the source. So that parsed phrases can live inside config structs, an owner type
pairs the pinned source string with a `'static`-lifetime AST — the same self-referential pattern as
`filt_rs::Filter`, and the only `unsafe` in the crate:

```rust
// grammar/mod.rs
pub struct CommandPhrase {
    source: std::pin::Pin<Box<String>>,
    expr: PhraseExpr<'static>,             // borrows from `source`; narrowed on access
}

impl CommandPhrase {
    pub fn parse(source: String) -> Result<Self, crate::Error>;
    pub fn source(&self) -> &str;
    pub fn expr(&self) -> &PhraseExpr<'_>;
}

impl Clone for CommandPhrase { /* re-parses source, like filt-rs */ }
impl std::fmt::Display for CommandPhrase { /* round-trips via the AST */ }
impl<'de> serde::Deserialize<'de> for CommandPhrase { /* parse during load */ }
```

Because `Deserialize` parses immediately, a bad phrase is a **config-load error** with a precise
location — never a runtime surprise.

### Error messages

Errors follow the filt-rs voice: second person, the location interpolated into the message (advice
arrays must be `&'static`, so all dynamic detail lives in the message), and a worked example wherever
it helps. Representative set:

| Situation | Message | Advice |
|---|---|---|
| Unclosed `[` | `You have an unclosed '[' at line 1, column 8 — every optional group needs a matching ']'.` | `Close the optional group, e.g. 'deploy [the] sentry'.` |
| Stray `]` | `We found a ']' at line 1, column 3 without a matching '[' before it.` | `Remove the stray ']' or add a '[' where the optional words begin.` |
| Empty alternate branch | `The alternates group starting at line 1, column 8 has an empty branch (a ',' with nothing before or after it).` | `Every branch in an '{a, b}' group needs at least one word, e.g. '{autocannon, auto cannon}'.` |
| Empty optional | `The optional group at line 1, column 8 is empty.` | `Put at least one word inside '[...]', or remove the brackets entirely.` |
| Depth limit | `You've nested groups more than 8 levels deep at line 1, column 41.` | `Simplify the phrase — deeply nested '[{...}]' groups usually read better as several separate commands.` |
| Invalid character | `We found an unexpected character '(' at line 1, column 12.` | `Phrases may only contain words, '[optional]' groups and '{alternate, choices}' groups.` |

Tests assert these by substring, including the exact `line X, column Y`, so location regressions are
loud.

### Expansion and grammar compilation

`grammar/expansion.rs` turns an AST into the concrete phrases Vosk will hear:

```rust
pub const MAX_EXPANSIONS_PER_COMMAND: usize = 512;

pub struct Expansion {
    pub phrases: Vec<Vec<String>>,   // deduped, insertion-ordered, lowercased word sequences
}

pub fn count(expr: &PhraseExpr<'_>) -> usize;                 // multiplicative, no materialization
pub fn expand(expr: &PhraseExpr<'_>) -> Result<Expansion, crate::Error>;
pub fn word_set(expr: &PhraseExpr<'_>) -> BTreeSet<String>;   // linear in AST size, for validate
```

Expansion is a cartesian product: `Optional` contributes the branches `{[], inner}`, `Alternates`
contributes its branches, and results are deduped with an insertion-ordered set (so phrases like
`[a] [a]` or `{a, a}` collapse their duplicates). A phrase whose terms are all optional expands to
include the *empty* phrase, which cannot be spoken — grammar compilation must reject such commands
(and `validate` lints them). The count is computed *multiplicatively before materializing*, so an
explosive phrase fails fast instead of allocating; exceeding `MAX_EXPANSIONS_PER_COMMAND` is a hard
error in both `validate` and `run` — silently truncating a grammar would silently break commands.
`word_set` walks the AST linearly, so vocabulary checking never needs the full expansion.

The grammar handed to `Recognizer::new_with_grammar` is: every expanded phrase of every command,
space-joined, globally deduped — **plus the special `"[unk]"` entry**. Without `[unk]`, Vosk
force-aligns *any* speech (including unrelated chatter on voice comms) onto the nearest grammar
phrase, causing false triggers; with it, out-of-grammar audio decodes as `[unk]` and the matcher
discards it.

## Matcher: trie + completion timeout

### Phrase table

All expanded phrases are loaded into a word-level trie (arena-allocated, indices instead of boxes):

```rust
// matcher/trie.rs
pub struct PhraseTrie { nodes: Vec<TrieNode> }        // index 0 = root

struct TrieNode {
    children: HashMap<String, usize>,                 // word → node index
    terminal: Option<CommandId>,                      // a command's full phrase ends here
}

pub struct CommandId(pub usize);                      // index into Vec<CompiledCommand>
```

Because one command expands to many phrases, many leaves map to the same `CommandId`. Ambiguity is a
*node* property: a node that is terminal **and** has children marks an utterance that is also a
strict prefix of some longer phrase — regardless of which commands are involved. Building the trie
detects duplicate phrases across commands (an error in `run`, a warning in `validate`).

### The completion-timeout state machine

The matcher consumes `RecognitionEvent`s and produces `CommandAction`s onto the command queue:

```rust
pub struct CommandAction {
    pub command: String,             // display name, for logging
    pub output: CompiledOutput,      // pre-compiled at load time
}

enum MatchState {
    Idle,
    Pending {
        command: CommandId,          // matched and ready to fire
        node: usize,                 // trie position to continue from
        deadline: tokio::time::Instant,   // now + profile.completion_timeout
    },
}
```

Commands fire on **Final** results only; partials are used solely to hold a pending timer open,
never to fire. Transitions:

- **Idle + Final(words):** strip `[unk]` tokens, then walk the words from the root with greedy
  longest-match segmentation — when a word has no child but the path passed a terminal, emit that
  terminal's command and re-sync the remaining words from the root; with no terminal on the path,
  drop up to the current word and re-sync (logged at debug). At the end of the words:
  - resting on an **unambiguous terminal** → fire immediately, go Idle;
  - resting on an **ambiguous terminal** → go `Pending { command, node, deadline: now + timeout }`;
  - resting mid-trie with no terminal → incomplete phrase, drop, go Idle.
- **Pending + timer fires:** fire the pending command, go Idle.
- **Pending + Final(words):** continue the walk from `node`. If the continuation reaches a longer
  terminal, the longer command *supersedes* the pending one (only the longer fires). If the first
  word does not extend from `node`, fire the pending command first, then match the new words from the
  root.
- **Pending + Partial(text):** if the partial's next word extends from `node`, push the deadline out
  to `now + timeout` — the speaker is mid-way through the longer phrase; don't fire the short command
  under them. Otherwise ignore.
- **Any + Muted:** clear everything, including a pending command — a half-confirmed command must not
  fire when listening resumes.

The loop is a single `select!`:

```rust
loop {
    tokio::select! {
        _ = cancel.cancelled() => break,
        _ = sleep_until_deadline(&state) => fire_pending(&mut state, &queue).await,
        ev = events.recv() => match ev {
            Some(ev) => transition(&mut state, ev, &trie, &queue).await,
            None => break,
        },
    }
}
// sleep_until_deadline: sleep_until(deadline) when Pending, std::future::pending() when Idle
```

### Endpointing and latency

Vosk finalizes an utterance after its internal silence endpoint (~0.5 s; not configurable through
the `vosk` 0.3 crate). Two honest consequences:

1. "autocannon sentry" spoken in one breath arrives as a *single* Final — the completion timeout
   never engages, and the command fires as fast as Vosk can finalize.
2. The timeout only matters when the speaker *pauses* between "autocannon" and "sentry". In that
   case the perceived latency for the short command is `endpoint silence + completion_timeout`.

That trade-off is correct for v1: it prioritizes never firing the wrong command over shaving
milliseconds. Partial-driven eager firing (executing off stable partials before the endpointer
closes) is the designed escape hatch if the latency proves objectionable, and is confined to the
matcher module.

## Profile schema

Profiles are standalone YAML files. serde does all structural validation at load (phrases parse
during deserialization, key names resolve during deserialization, durations parse via humantime);
explicit `validate_*()` methods cover cross-field invariants and always name the offending command.

```yaml
# examples/profile.yaml
name: Deep Rock Galactic
model: ~/.local/share/vosk/vosk-model-small-en-us-0.15   # ~ expanded at load

audio:
  device: default          # or a substring of the cpal device name, e.g. "USB Microphone"

hotkey:
  device: auto             # auto | /dev/input/event3 | device-name substring
  key: rightctrl           # friendly key name (see key reference)
  mode: toggle             # toggle | push-to-talk | push-to-mute
  interrupt: false         # true: stopping listening also cancels the command being typed

completion_timeout: 350ms  # ambiguous-prefix settle time (default: 300ms)

defaults:                  # applies to the `keys:` shorthand form
  duration: 30ms           # how long each chord is held down
  interval: 25ms           # gap between chords

commands:
  - phrase: deploy [the] {autocannon, auto cannon} [sentry]
    keys: ["4"]                        # shorthand: press sequence

  - phrase: open [the] terminal
    keys: ["leftctrl+leftalt+t"]       # chord: all down in listed order, all up in reverse

  - name: Salute                       # optional display name (defaults to the phrase)
    phrase: salute
    events:                            # explicit form: full control
      - down: x
      - wait: 750ms
      - up: x
```

`hotkey.interrupt` is the output-side half of a mute. Muting always resets the recognizer and clears
the matcher's pending state, so nothing half-spoken survives it; with `interrupt: true` the executor
goes the rest of the way, abandoning the plan it is mid-way through (including from under a `Wait`),
releasing every key it holds, and discarding everything the matcher had already queued behind it.
The default, `false`, lets an in-flight command play out in full — a stratagem input which is a
single indivisible macro should not be left half-entered because you let go of push-to-talk a beat
early.

### System configuration

A profile says *what to listen for*; a machine-level configuration file says *what to listen with*.
The split is what makes a profile shareable: a published Helldivers 2 profile should not carry one
person's microphone name, one person's keyboard and one person's model directory with it.

```yaml
# ~/.config/voice-orders/config.yaml  ($XDG_CONFIG_HOME/voice-orders/config.yaml when that is set)
audio:
  device: "USB Microphone"     # default audio input when a profile doesn't specify one
hotkey:                        # default hotkey when a profile doesn't specify one (field-level merge)
  device: auto
  key: rightctrl
  mode: push-to-talk
  interrupt: false
models:
  path: ~/.local/share/vosk    # directory where profiles' model *names* are resolved
```

Every field is optional, and so is the file: an absent config is exactly `SystemConfig::default()`,
which reproduces the behaviour voice-orders had before it existed. It parses under the profile's
conventions — `deny_unknown_fields`, the same `KeyName` deserializer, `~` expanded at load — and an
unreadable or malformed file is a user error naming the path, surfaced by whichever command loaded
it. `SystemConfig::load()` takes its location from a `Paths` struct (the pattern `setup` uses), so
tests inject a temporary directory and never touch a real `~/.config`.

Resolution happens in exactly one place — `ResolvedSettings::resolve(&profile, &system)` in
`config/system.rs` — which `run`, `test` and `doctor` all call, so the three commands cannot
disagree about which microphone or hotkey you meant:

- **Audio.** `Profile.audio.device` is an `Option<String>`: the profile's value, else the system
  config's `audio.device`, else `"default"`.
- **Hotkey.** Every field of the `hotkey:` block is optional, `key` included, and the profile and
  system blocks are merged **field by field**: for each of `device`, `key`, `mode` and `interrupt`,
  the profile's value if it set one, else the system config's, else the schema default (`auto`,
  `toggle`, `false`). The hotkey is active **iff a `key` emerges from the merge** — so a shared
  profile can omit the block entirely and pick up whichever key this machine has chosen. A profile
  which *writes* a `hotkey:` block without a key emerging anywhere is a config error naming the
  missing field; a system config which offers a keyless hotkey to a profile which asked for none
  leaves it listening continuously, as before.
- **Models.** `model:` (and `--model`) may be a bare **name** — no `/`, no leading `~` or `.` — in
  which case it resolves to `<models.path>/<name>`, with `models.path` defaulting to
  `~/.local/share/vosk`. Anything path-like keeps its current meaning. The resolution *order* is
  unchanged (`--model` → profile → `$VOSK_MODEL_PATH`); `resolve_model` simply takes the system
  config as a third argument, and its "no model anywhere" error now also names the models directory
  and the name form.

`doctor` prints which configuration file it loaded (or that there is none) above its checks, and
reports the *merged* values: check 4 resolves the microphone the run would actually open, and check
6 names the merged hotkey.

### Output forms

`keys` and `events` are mutually exclusive per command (validated with an error naming the command).
Both compile at load time into a flat event plan:

```rust
// output/mod.rs
pub enum CompiledOutput {
    Keyboard(Vec<KeyEvent>),
    // future: Mouse(..), Exec(..), Audio(..)
}

pub enum KeyEvent { Down(KeyCode), Up(KeyCode), Wait(std::time::Duration) }
```

- **`keys` (shorthand):** each entry is a chord like `"leftctrl+shift+p"` or a single key `"4"`.
  Compilation per chord: `Down` each key in listed order → `Wait(duration)` → `Up` each key in
  reverse order → `Wait(interval)` before the next chord. `duration`/`interval` come from
  `defaults`, overridable per command.
- **`events` (explicit):** 1:1 with `KeyEvent`. An unmatched `Down` is legal (hold-style macros) but
  linted by `validate`, as is an `Up` without a prior `Down`.

### Key naming

Friendly key names are the lowercase evdev constant names with the `KEY_` prefix stripped: `a`, `1`,
`f5`, `space`, `enter`, `esc`, `leftctrl`, `leftshift`, `leftalt`, `leftmeta`, `rightctrl`, `up`,
`kp1`, … One macro-generated table in `output/keys.rs` is the single source of truth:

```rust
macro_rules! keys { ($($name:literal => $evdev:ident / $uinput:ident),* $(,)?) => { /* ... */ } }

pub struct KeyCode(u16);                            // raw evdev code
pub fn from_name(name: &str) -> Option<KeyCode>;    // used by Deserialize
pub fn to_uinput(code: KeyCode) -> uinput_tokio::event::keyboard::Key;
pub fn to_evdev(code: KeyCode) -> evdev::KeyCode;   // hotkey matching
pub fn all_names() -> &'static [&'static str];      // docs generation + suggestions
```

An unknown key name fails deserialization with a human error whose message includes a
`strsim`-ranked "did you mean 'leftctrl'?" (advice stays static, pointing at the key reference docs
page).

## CLI

```rust
#[derive(Parser)]
#[command(version, about)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Scaffold a new profile YAML with commented examples.
    New { profile: PathBuf },
    /// Check a profile: grammar, key names, and every word against the Vosk model.
    Validate {
        profile: String,                   // String: may be a path or an https URL
        /// Override the model (else: profile `model:`, else $VOSK_MODEL_PATH).
        #[arg(long)]
        model: Option<PathBuf>,
    },
    /// Listen and report what would happen, without emitting any input events.
    /// Runs the full audio → recognition → matcher pipeline (including the
    /// hotkey, so the keybind itself can be exercised), but prints each heard
    /// utterance, each matched command with the key plan it would have played,
    /// and listening-state changes to the terminal instead of opening
    /// /dev/uinput — so a profile can be rehearsed before launching a game,
    /// and before uinput permissions are even set up. Ctrl-C exits.
    Test {
        profile: String,
        /// Override the model (else: profile `model:`, else $VOSK_MODEL_PATH).
        #[arg(long)]
        model: Option<PathBuf>,
    },
    /// List this machine's microphones and input devices, as `audio.device`
    /// and `hotkey.device` see them: copy-paste-ready names, the system
    /// default microphone marked, and every readable /dev/input/event* with
    /// its keyboard rank and the one `device: auto` would pick.
    Devices {
        /// Only list audio input devices.
        #[arg(long)]
        audio: bool,
        /// Only list hotkey (evdev) input devices.
        #[arg(long)]
        hotkey: bool,
    },
    /// Load a profile and start listening; optionally wrap a child process.
    Run {
        profile: String,
        /// Application to launch; voice-orders exits when it exits.
        /// Steam: `voice-orders run profile.yaml -- %command%`
        #[arg(last = true)]
        app: Vec<String>,
        /// Print recognition events (partials/finals) for debugging.
        #[arg(long, hide = true)]
        debug_recognition: bool,
    },
}
```

`new` writes a scaffold with every option present-but-commented plus two example commands, and
refuses to overwrite an existing file (user error with advice).

### `devices`: what this machine has to listen with

Both `audio.device` and `hotkey.device` are matched against strings only the machine knows, and
every failure to match them ends in somebody guessing. `voice-orders devices` prints the two lists
in the form the options take — plain `println!`, not a TUI — so a value can be copied straight out:

- **Audio inputs** — every cpal input device name, with the system default marked `*` and the
  machine's configured `audio.device` annotated (including when it matches nothing, which is exactly
  the stale-config case worth surfacing).
- **Hotkey devices** — every readable `/dev/input/event*`: path, name, and its keyboard rank from
  `hotkey/discovery.rs`, with `*` on the device `device: auto` would settle on for a typical key
  (left control). The ranking and the choice come from the discovery module itself
  (`list_devices`/`auto_choice`), so the listing cannot drift away from what a real run does.

The two sections are independent: each reports its own failure inline — `/dev/input` permission
problems get the same human advice discovery gives — and the exit code is 1 only when *every*
section shown failed. `--audio` and `--hotkey` narrow it to one section. The rendering is a pure
function of a device list plus ranks, which is what makes it unit-testable without hardware.

### The `test` terminal UI (ratatui)

`voice-orders test` renders as a full-screen terminal UI (ratatui + crossterm) rather than a line
printer, keeping the rehearsal loop glanceable mid-game-setup. Layout, kept deliberately simple:

- **Header** — the active profile name (left-aligned) with its stats (command count, phrase count,
  hotkey mode) and the profile source (the resolved file path or https URL).
- **Body** — a scrolling event log where each line leads with a colored severity dot: green `●` for
  a matched command (with the key plan it would have played), grey `●` for a heard-but-unmatched
  utterance, yellow `●` for an interrupted/discarded command, red `●` for pipeline errors, blue `●`
  for listening-state changes.
- **Footer** — the live listening state, and the loaded model name right-aligned.

`q`/`Ctrl-C` exits. When stdout is not a TTY (piped output, CI), `test` falls back to the plain
line-printed report so scripts keep working. The TUI is `test`-only for now; extending it to `run`
is future work.

### `setup` and `doctor`: system configuration & diagnostics

The kernel-level approach needs one-time system configuration (udev rule, `uinput` module, `input`
group membership). Two subcommands make that a guided experience instead of a docs page:

**`voice-orders doctor [profile]`** — read-only diagnosis. Runs each check, printing a `✓`/`✗` line
with human-errors advice on failure; exit 1 iff any check fails:

1. `/dev/uinput` exists (else: the `uinput` module is not loaded — advise `setup`).
2. A virtual keyboard can actually be created (opens and immediately destroys a uinput device —
   the definitive permissions test).
3. The user is in the `input` group, and at least one `/dev/input/event*` keyboard device is
   readable (reusing the hotkey discovery path).
4. An audio input device is present (cpal enumeration).
5. `libvosk.so` loads, reporting where it was found (see below).
6. A speech model resolves (CLI → profile → `$VOSK_MODEL_PATH`) and has a dynamic graph
   (`graph/Gr.fst`), i.e. supports grammar mode.
7. With a profile argument: the profile loads, and its hotkey device resolves.

Group membership changes need a re-login; `doctor` distinguishes "not in the group" from "in the
group but this session predates the change" (effective vs. configured groups) and says which.

**`voice-orders setup`** — applies the system configuration `doctor` checks for. It first runs the
relevant checks, prints exactly what it intends to change (only the missing pieces), and asks for
confirmation (`--yes` skips, `--print` prints the equivalent shell commands and exits without
changing anything):

1. `/etc/udev/rules.d/60-voice-orders-uinput.rules` — `KERNEL=="uinput", GROUP="input", MODE="0660"`.
2. `/etc/modules-load.d/voice-orders.conf` containing `uinput`, plus `modprobe uinput` now.
3. `usermod -aG input <user>`.
4. `udevadm control --reload-rules && udevadm trigger`.

When not running as root, each step is executed through `sudo` (spawned interactively so the
password prompt works); the final message reminds the user that group membership takes effect at
next login and suggests `voice-orders doctor` to verify.

## `run` assembly & child processes

Assembly order in `commands/run.rs` — each step fails early with a human error:

1. Load and validate the profile (path or URL via `config/loader.rs`).
2. Expand grammars; build the `PhraseTrie` and `Vec<CompiledCommand>`; duplicate phrases and
   expansion-cap violations are errors here.
3. **Create the uinput device first** — this fails fast on missing permissions before any audio
   machinery spins up. The virtual device is named `voice-orders` and registers the *full* key
   capability set from `keys.rs`, which makes it look like a real keyboard to SDL and games.
4. Load the Vosk model and spawn the recognizer thread with the compiled grammar.
5. Start the cpal stream; spawn the hotkey, matcher, and executor tasks; wire the channels. Initial
   listening state: `false` for toggle and push-to-talk, `true` for push-to-mute. With
   `hotkey.interrupt: true` the executor also gets its own `watch::Receiver<bool>` subscription to
   the listening state (`Interrupt::WhenListeningStops`), so a mute cancels the command in flight
   and drains the queue instead of only silencing the microphone; every other profile gets
   `Interrupt::Never`, which costs one never-ready `select!` arm and nothing else.
6. If `app` is non-empty, spawn it with `tokio::process::Command` (stdio inherited).
7. Supervise:

```rust
tokio::select! {
    status = child.wait(), if child.is_some() => { cancel.cancel(); exit_code = status.code().unwrap_or(1); }
    _ = tokio::signal::ctrl_c() => cancel.cancel(),           // child shares our process group;
                                                              // the kernel already delivered SIGINT to it
    _ = sigterm.recv() => { cancel.cancel(); forward_sigterm(&child); }  // Steam shutdown: libc::kill,
                                                              // then a 5s grace before exiting anyway
}
```

**Shutdown order matters:** cancel token → cpal stream dropped → audio channel closes → recognizer
loop ends and joins → matcher and executor drain → **the executor releases every key it still holds
down** (it tracks a `HashSet<KeyCode>` of pressed keys and emits `Up` + synchronize for each on
cancellation — a voice macro must never leave W held down in a game) → uinput fd closes (the kernel
removes the virtual device) → the process exits with the child's exit code, preserving the Steam
wrapper contract.

## Loading profiles from URLs

`config/loader.rs` resolves the `profile` argument:

- `https://…` → fetch with reqwest (rustls, no default features).
- `http://…` → user error: profiles are only fetched over HTTPS (advice: use https, or download the
  file manually).
- anything else → local path, with `~` expansion.

Conveniences and guards:

- A `gist.github.com/...` URL without `/raw` gets `/raw` appended (which resolves to the latest
  revision's first file); `gist.githubusercontent.com` and `raw.githubusercontent.com` URLs pass
  through untouched.
- A body starting with `<html` or `<!DOCTYPE` fails with a user error advising the *raw* URL — the
  classic copy-the-pretty-gist-page mistake.
- All network failures flow through `errors::HumanizableError` exactly as in github-backup: connect
  and timeout failures are `User`-kind (actionable, not paged), everything unexpected is `System`.

**No caching in v1.** A `run` at game-launch time should fail loudly rather than silently run a stale
profile; the recommended workflow for flaky networks is downloading the profile to a local file. An
XDG-cache offline fallback is noted as future work.

## `validate`

Pipeline: load the profile (serde already validated structure, phrases, key names, and durations) →
cross-field `validate_*()` checks → grammar lints → vocabulary check. Output is one section per
command; failures render as `human_errors::pretty` user errors, lints as `warning:`/`note:` lines.
All problems are reported in a single pass; the exit code is 1 iff any error occurred.

### Vocabulary checking

Implemented against the `Vocabulary` trait (production: `Model::find_word`; tests: a HashSet-backed
fake). For each unique word from `word_set` (linear — no expansion blow-up), check membership. For
misses, suggest in order:

1. **Normalization** — lowercase and strip punctuation (`.,!?'"`); if the normalized form is known,
   suggest it. (Expansion already lowercases; this catches punctuation pasted into phrases.)
2. **Compound decomposition** — for every split point `i`, if both `word[..i]` and `word[i..]` are in
   the vocabulary, suggest the split (e.g. `autocannon` → `"auto cannon"`). Report at most two
   splits, most-balanced first.
3. **Nearest words** — if the model ships a readable word list (`<model>/graph/words.txt`), rank
   candidates by `strsim` Levenshtein distance (≤ 2, preferring a shared first letter) and report the
   top three. Models without a word list get a note that nearest-word suggestions are unavailable.

### Lints

- **Duplicate phrase across commands** (from trie construction): warning here, error in `run` — so a
  single `validate` run shows every problem at once.
- **Prefix relations:** for every ambiguous trie node, a note such as
  `note: saying "autocannon" will wait 350ms in case you continue with "autocannon sentry"` — making
  the completion-timeout behavior discoverable per profile.
- **Expansion volume:** per-command counts; warning above 128, error above 512.
- **Output lints:** `down` without a matching `up`, `up` without a prior `down`, empty `keys` lists.

## libvosk & model distribution

There is no pure-crates.io path to Vosk: `libvosk.so` has to be on the machine.

### Loading it

`vosk-sys` binds the C API in an `extern` block carrying `#[link(name = "vosk")]`, which puts a
`DT_NEEDED` entry in the executable — so the dynamic loader resolves libvosk *before* `main`, and a
machine without it cannot run voice-orders at all. Not `--version`, not `setup`, and not `doctor`,
whose whole job is to explain what is missing; the user gets
`error while loading shared libraries: libvosk.so` and nothing else.

`recognition/libvosk.rs` therefore owns the FFI itself: the fifteen entry points we use are declared
as function-pointer fields, and the library is `dlopen`ed on first use (`RTLD_NOW | RTLD_LOCAL`) into
a process-lifetime `OnceLock`. A missing library becomes an ordinary `human_errors` **user** error
carrying install instructions, raised where recognition is set up — every other command keeps
working, and `doctor` gets to report it as one more `✗` line. `recognition/vosk.rs` sits on top of
that with the safe `Model`/`Recognizer` wrappers and the decoder thread, unchanged in shape.

Search order: `$VOSK_LIB_PATH` (the library, or the directory holding it), then the bare `libvosk.so`
so that `dlopen` searches the binary's `RUNPATH` — `$ORIGIN`, `$ORIGIN/../lib` and
`$HOMEBREW_PREFIX/lib` — followed by `$LD_LIBRARY_PATH`, the `ldconfig` cache and the system
directories.

### Model selection

Grammar-constrained recognition (`Recognizer::new_with_grammar`) requires a model with a **dynamic
graph** (`graph/Gr.fst` + `graph/HCLr.fst`). That constrains which models this tool supports:

| Model | Size | Dynamic graph | Verdict |
|---|---|---|---|
| `vosk-model-small-en-us-0.15` | ~40 MB | yes | works; smallest vocabulary |
| `vosk-model-en-us-0.22-lgraph` | ~128 MB | yes | **recommended** — much larger vocabulary, still grammar-capable |
| `vosk-model-en-us-0.22` (static) | ~1.8 GB | no | rejected — grammar mode is unavailable, which defeats the design |

The recognizer fails loudly with a "this model cannot be constrained to a grammar" user error when
pointed at a static-graph model, advising a dynamic-graph alternative. The docs recommend the
`lgraph` model as the default for gaming profiles: its larger vocabulary makes far more phrase words
recognizable (fewer `validate` misses), at a still-modest download size.

Model resolution order (so shared profiles need not hard-code paths):

1. the `--model <path>` CLI override on `run` and `validate`;
2. the profile's `model:` field (`~` expanded);
3. the `VOSK_MODEL_PATH` environment variable.

If none is set, the error advises downloading a model and lists the three mechanisms.

- **Docs:** a dedicated installation page covers the Homebrew tap, the raw release assets, installing
  `libvosk.so` (`/usr/local/lib` + `ldconfig`, `$(brew --prefix)/lib`, or beside the binary), and
  downloading/unpacking a model (small-en-us is ~40 MB).
- **CI:** only the jobs which need the library at *runtime* fetch it. `test` downloads and caches the
  libvosk zip and exports `VOSK_LIB_PATH`/`LD_LIBRARY_PATH`; `build` fetches it purely to publish it
  as a release asset; `check` needs it not at all, because nothing links against it. The model itself
  is cached and downloaded only in the test job.
- **Feature gate:** following the github-backup `pure_tests` pattern, tests that need a real model
  (vocabulary integration, end-to-end `validate examples/profile.yaml`) are marked
  `#[cfg_attr(feature = "pure_tests", ignore)]`; everything else runs against the trait fakes and
  needs no `.so` at all.
- **Releases:** the binary and `libvosk.so` are published as raw, unarchived assets
  (`voice-orders-linux-<arch>` and `libvosk-linux-<arch>.so`) following the Sierra Softworks
  `{app}-{os}-{arch}` convention, which is also what the Homebrew tap consumes. The binary is built
  with `-C link-args=-Wl,-rpath,$ORIGIN,-rpath,$ORIGIN/../lib,-rpath,$ORIGIN/../../../../lib` so the
  two find each other side by side, under a `bin`/`lib` prefix, or in a Homebrew Cellar. Vosk is
  Apache-2.0; redistribution is fine.
- **Tap:** a release-only fan-in job (`SierraSoftworks/actions-tap`, `name: voice-orders` because the
  repository is `voice-rs`) rewrites the formula in `SierraSoftworks/homebrew-tap` with `major minor`
  aliases. The formula installs the binary alone — libvosk goes into `$(brew --prefix)/lib`, which
  the rpath above covers.

## Testing strategy

- **Lexer:** a filt-rs-style `assert_sequence!` macro matching token streams by pattern (eliding
  `Loc` with `..`); rstest tables; invalid-character errors asserted with exact line/column.
- **Parser:** whole-AST equality tables (`PhraseExpr: PartialEq`); every error-message row from the
  table above asserted by substring; a 9-deep nesting rejection test and a 500-word long-phrase
  test; `Display` round-trip and Clone-reparses tests for `CommandPhrase`.
- **Expansion:** phrase → expected-list tables; the `[a] {a, b}` dedupe case; a cap short-circuit
  test proving an explosive phrase errors fast without materializing.
- **Matcher:** `#[tokio::test(start_paused = true)]` + `tokio::time::advance`, feeding synthetic
  `RecognitionEvent`s through the real task and asserting `CommandAction` order and timing:
  unambiguous immediate fire; ambiguous → timeout fire; ambiguous → superseding continuation;
  partial extends the deadline; non-extending Final flushes then rematches; `Muted` clears pending;
  `[unk]` stripped; multiple commands in one utterance.
- **Executor:** a `KeySink` trait over the uinput wrapper; a fake sink records `(event, instant)`
  under paused time to assert duration/interval timing, plus the pressed-keys-released-on-cancel
  guarantee.
- **Config:** `examples/profile.yaml` loaded in a unit test (the docs-can't-drift trick from grey
  and github-backup); bad DSL inside YAML surfaces a located load error; both output forms compile
  to the expected `Vec<KeyEvent>`; mutual-exclusion errors name the command.
- **Keys table:** every name round-trips name → code → uinput/evdev; suggestion quality spot checks.
- **Validate:** fake `Vocabulary` covering normalization, compound splits, and nearest-word ranking
  with an injected word list; lint output assertions.
- **Loader:** wiremock for fetch success/404/HTML-body/gist-rewrite; tempfile for path handling.
- **Gated (`pure_tests`):** real-model `find_word` sanity, real grammar construction, end-to-end
  validate against the small English model in CI.

## Risks & open questions

1. **Vosk endpointing is not configurable** through the 0.3 crate; the latency trade-off is
   documented under [Endpointing](#endpointing-and-latency), with partial-driven matching as the
   escape hatch. The exact `DecodingState` variant names should be re-verified when implementation
   starts.
   *Discovered during implementation:* `Recognizer::reset()` alone does **not** discard a
   mid-utterance decode — the partial reads empty afterwards, but the stale utterance can still
   finalize off subsequent audio (verified empirically with libvosk 0.3.45; this is what made a
   push-to-talk release leak a command across the mute boundary). The recognizer therefore drains
   `final_result()` before every reset (`discard_utterance` in `recognition/vosk.rs`), the
   listening bridge sends `AudioMsg::Clear` on the unmute edge as well, and a regression test with
   real recorded speech pins the behavior.
2. **Permissions.** `/dev/uinput` needs the `uinput` module plus a udev rule
   (`KERNEL=="uinput", GROUP="input", MODE="0660"`), and `/dev/input/event*` needs membership in the
   `input` group — one group covers both sides. First-run failures must print the exact udev rule in
   the error message, with docs-link advice.
3. **Privacy candor.** evdev hotkeys are truly global (they fire even while typing in a password
   field), and a process reading `/dev/input` can technically observe all keystrokes. The docs state
   plainly that only the configured hotkey is inspected.
4. **Games vs virtual devices.** SDL sometimes filters input devices, and some anti-cheat systems
   flag uinput. Registering the full key capability set with a keyboard-like name mitigates the
   former; the latter is documented, not solved.
5. **cpal format variance under PipeWire** — the naive decimation path may need `rubato` if
   recognition accuracy disappoints; the change is isolated to `audio/`.
6. **Grammar scale** is bounded by the per-command cap; thousands-of-phrases profiles are out of
   scope for v1, as is runtime grammar switching.
7. **`uinput-tokio` coverage** — verify its keyboard event enum covers the whole `keys.rs` table;
   the wrapper isolates a raw-`EV_KEY` fallback if any name is missing.

## Implementation milestones

| # | Milestone | Contents |
|---|---|---|
| M1 | Grammar core | `src/grammar/*` (lexer, parser, AST, expansion, errors), `errors.rs`/`macros.rs` skeletons, `main.rs` stub, Cargo.toml, CI check+test. All parser/expansion tests green. |
| M2 | Config + CLI skeleton | `src/config/*` (minus loader), `output/keys.rs`, `commands/{new,validate}` (structural validation + lints, no vocabulary), `examples/profile.yaml` + its test, telemetry wiring. |
| M3 | Recognition + audio | `recognition/*` (traits + Vosk thread), `audio/mod.rs` (cpal), vocabulary checks + suggestions completing `validate`, CI libvosk/model steps, `pure_tests` gating, `--debug-recognition`. |
| M4 | Matcher + executor + uinput | `matcher/*`, `output/{mod,uinput}.rs`, first `run` assembly (always-listening), CancellationToken plumbing, key-release-on-shutdown, paused-time tests, real-game smoke test. |
| M5 | Hotkey | `hotkey/mod.rs`: evdev discovery, EventStream task, three listen modes, Reset/Muted wiring, permission errors. |
| M6 | Child process + URL profiles | Spawn/supervise/signal-forwarding/exit-code propagation; `config/loader.rs` with wiremock tests; Steam docs snippet. |
| M7 | Docs + release | VuePress site, Pages workflow, release-drafter, nightly audit, release matrix with libvosk-bundled rpath tarballs, version-from-tag. |

## Documentation site

A VuePress 2 site under `docs/`, following grey's template (config in `docs/.vuepress/config.ts`,
sidebar keyed by URL prefix, `<Badge>`-annotated option references, YAML examples with line
highlighting, deployed with `actions/upload-pages-artifact` + `actions/deploy-pages`):

- **Guide** — getting started, installation (libvosk, models, udev rules and the `input` group),
  Steam integration.
- **Profiles** — the full option reference (each option an `###` heading with a required/default
  Badge), the two output forms, hotkey modes.
- **Grammar** — the phrase DSL with worked examples, ambiguity and the completion timeout, how
  validation suggestions work.
- **Keys** — the key-name reference, generated from `keys::all_names()` so it cannot drift.
