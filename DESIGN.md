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
  arbitrary nesting of the two. (Since superseded by the composable rule grammar — see
  [Composable command grammars](#composable-command-grammars).)
- Ambiguous-prefix commands ("autocannon" vs "autocannon sentry") resolved by a configurable
  statement-completion timeout.
- Low-latency recognition: a configurable endpointer, partial-driven ("eager") command firing, and
  optional n-best confidence gating — see [Endpointing](#endpointing-and-latency).
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
- Windows/macOS support. This tool is deliberately Linux-first at the evdev/uinput layer. (A
  Windows port is now underway — see [Windows support](#windows-support) — on the standing condition
  that it never compromises the Linux one. macOS remains out of scope.)

(Partial-result "eager" command firing was originally listed here as future work; it has since been
implemented — see [Endpointing](#endpointing-and-latency).)

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
    │   ├── new.rs                 # scaffold a commented profile with a worked grammar
    │   ├── validate.rs            # structure + grammar analysis + vocabulary
    │   └── run.rs                 # pipeline assembly + child process supervision
    ├── config/
    │   ├── mod.rs                 # Profile (serde), cross-field validate_*() methods
    │   ├── hotkey.rs              # HotkeyConfig { device, key, mode }
    │   ├── output.rs              # KeyName resolution + OutputDefaults pacing
    │   ├── duration.rs            # humantime deserialize_with helpers
    │   └── loader.rs              # path-vs-https detection, reqwest fetch, gist raw rewrite
    ├── grammar/
    │   ├── mod.rs                 # Grammar: parse-and-analyze entry point, word set, serde
    │   ├── token.rs               # the token vocabulary
    │   ├── lexer.rs               # chumsky lexer → spanned token stream
    │   ├── parser.rs              # chumsky parser → spanned rule list
    │   ├── ast.rs                 # owned, spanned AST (rules/branches/terms/actions)
    │   ├── analysis.rs            # static analysis: errors + lints
    │   ├── diagnostic.rs          # Diagnostic + ariadne rendering + user_error
    │   ├── automaton.rs           # NFA transducer compiler + hypothesis Walk
    │   └── feed.rs                # recognition feed: expansion or rule-boundary decomposition
    ├── audio/
    │   └── mod.rs                 # cpal capture: device selection, format conversion, frame channel
    ├── recognition/
    │   ├── mod.rs                 # Recognition/Vocabulary traits (mockable), RecognitionEvent
    │   └── vosk.rs                # Vosk Model/Recognizer wrapper + dedicated thread loop
    ├── matcher/
    │   ├── mod.rs                 # MatcherOptions/CommandAction: the engine's shared vocabulary
    │   └── engine.rs              # engine task: hypothesis walk + completion-timeout state machine
    ├── hotkey/
    │   ├── mod.rs                 # ListenMode logic + the platform-neutral watch() seam
    │   ├── discovery.rs           # evdev device discovery and ranking          [linux]
    │   ├── task.rs                # evdev EventStream task                      [linux]
    │   └── win.rs                 # WH_KEYBOARD_LL hook thread                  [windows]
    └── output/
        ├── mod.rs                 # executor task consuming the command queue
        ├── keys.rs                # KeyCode + name/uinput/Windows table (macro-generated)
        ├── uinput.rs              # uinput-tokio device wrapper                 [linux]
        └── sendinput.rs           # SendInput KeySink                           [windows]
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
chumsky = "0.13"                      # the grammar's lexer + parser
ariadne = "0.6"                       # grammar diagnostics with source excerpts

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
                 │      matcher task: hypothesis walk + completion-timeout      │
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
    Final(Utterance),  // utterance finalized by Vosk's endpointer
    Muted,             // listening turned off; matcher must clear all state
}

pub struct Utterance {
    pub text: String,                       // the 1-best transcript
    pub alternatives: Vec<(String, f32)>,   // n-best list; empty unless recognition.alternatives > 0
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

## Composable command grammars

> **Status: implemented.** This is the grammar the crate ships (the work plan at the end of this
> section records how it landed). It was a **breaking profile change**: the original single-phrase
> DSL, the `keys:`/`events:` output forms and the `commands:` list were removed rather than
> maintained alongside it. `profiles/arma3.yaml` is the canonical example, and every shipped
> profile is loaded and compiled by unit tests so the examples cannot drift from the code.

The original phrase DSL described single phrases; it could not share structure between commands. Articulate-
style grammars need exactly that: forty commands which all start with the same *subject* rule
("two and three fall back"), direct objects whose keys depend on context, and repetition. Grammar
v2 is a rule-based grammar language in which commands compose:

```
squad_number = ( "one" { f1 } | "two" { f2 } | "three" { f3 } )
squad_selection = squad_number ("and"? squad_number)[0..9]
subject = subject_all | squad_selection | team_selection

Advance = subject ("advance" | "move up") { ..., 1, 2 }
Watch = subject:sub ("watch" | "watch the") direction:dir { sub..., 3, 8, wait(20ms), dir... }
```

### Surface syntax

A grammar is a sequence of rules. **TitleCase rules are published** (speakable commands, named in
logs by their rule name plus captures, e.g. `Watch(two three, north)`); **lowercase rules are
private** building blocks. `//` comments run to end of line. A rule is
`name = expression [ { action-block } ]`; the expression grammar:

```
rule       = ident , "=" , alternation , [ actions ] ;
alternation= sequence , { "|" , sequence } ;
sequence   = term , { term } ;
term       = atom , [ repeat ] , [ ":" , ident ] ;
atom       = literal | ident | "(" , alternation , [ actions ] , ")" ;   (* inline actions bind
                                                                            to the branch they
                                                                            terminate *)
repeat     = "?" | "*" | "+" | "[" , bounds , "]" ;
bounds     = count | [ count ] , ".." , [ count ] ;
actions    = "{" , action , { "," , action } , "}" ;
action     = chord | "wait" , "(" , duration , ")"
           | "hold" , "(" , chord , ")" | "release" , "(" , ( chord | "*" ) , ")"
           | "..." | ident , "..." ;
```

- **Literals** are double-quoted spoken text; multi-word literals split on whitespace into word
  tokens, lowercased (words may contain letters, digits, `'` and `-`, as in the v1 DSL).
- **Repetition is always bounded.** `[n]` is exactly n, `[min..max]` a range, `[min..]` and
  `[..max]` fill the missing end with `0` / the global cap. `?`, `*`, `+` are sugar for `[0..1]`,
  `[0..cap]`, `[1..cap]`. The global cap (`MAX_REPETITION = 8`) makes every grammar finite by
  construction; `validate` reports per-rule automaton size so a `*` at the cap is visible.
- **No recursion.** A rule-reference cycle is a load error advising a bounded repetition instead.
- **Rule termination.** The action block ends a rule when present; a rule without one ends at the
  next `ident =` at the start of a line, and **implicitly propagates** its accumulated commands
  (equivalent to `{ ... }`).

### Command semantics

Matching a published rule accumulates a **command vector**: walking the match left to right, each
matched atom appends its commands (inline branch actions, referenced rules' vectors, nothing for
plain literals). Repetition iterations append in spoken order. The rule's action block — evaluated
only after the *whole published command* matches — then builds the vector the command actually
runs:

- a bare **chord** (`m`, `1`, `shift+f1`) is a key press;
- **`wait(20ms)`** an explicit pause, **`hold(chord)`** / **`release(chord)`** press/release
  without the paired edge, **`release(*)`** releases every key the virtual keyboard currently
  holds (the executor's tracked set — also what makes a panic rule possible);
- **`...`** splices the entire accumulated child vector, usable any number of times;
- **`name...`** splices a **capture**: `term:name` names any term (including a group:
  `("team"? assign_colour):colour`), and its commands accumulate into that capture's own
  `Vec` as they are matched — a capture in an unmatched optional is empty, a capture inside a
  repetition appends per iteration. Naming a capture does **not** remove it from `...`; a block
  using both bare `...` and a `name...` splices those commands twice, which is legal but almost
  always a mistake — lint it.

`wait`, `hold` and `release` are reserved words in action position; every other bare identifier
resolves against the `keys.rs` table (unknown names get the strsim "did you mean" treatment at
load). **Pacing**: the assembled vector is flattened, then `defaults.duration` is applied to each
press and `defaults.interval` between consecutive presses (across splice boundaries too); `hold`/
`release` carry no implicit pacing, and an explicit `wait` *replaces* the implicit interval at
that point.

### Parsing: chumsky + ariadne

The original zero-copy lexer/parser (and the `Pin<Box<String>>` self-referential owner, once the
only unsafe in the crate) gave way to the pattern proven in
[optimist's squiggle module](https://github.com/SierraSoftworks/optimist/blob/main/src/squiggle/mod.rs):
**chumsky 0.13** for a two-stage parse (lexer → spanned token stream → parser over
`Stream::from_iter`, `into_output_errors()` accumulating rather than aborting) and **ariadne 0.6**
for rendering. The AST is owned (`String` words, byte-offset `Span`s) — no borrowed lifetimes, no
unsafe. Errors are collected as `Diagnostic { kind, message, span, help }` values (syntax /
analysis / lint), rendered by ariadne with the source excerpt into plain UTF-8 text and carried to
the user inside a `human_errors::user` error, so `crate::Error` remains the only error type and
the advice-array convention is untouched. Every diagnostic keeps the second-person voice and a
worked example (`You have an unclosed '(' …` / `Close the group, e.g. ("fall back" | "regroup")`),
and the profile parses its grammar during deserialization, so a bad grammar is a config-load error
naming the profile's source — never a runtime surprise.

### Static analysis (load-time, all reported in one pass)

Errors: reference to an undefined rule; duplicate rule definition; rule-reference cycles; a
published rule that can match the empty word sequence; two published rules accepting the same word
sequence with different assembled outputs (checked on the automaton with a bounded subset
sweep — identical outputs may silently collapse, which is how deliberate synonyms are written); automaton size above `MAX_AUTOMATON_STATES`;
unknown key names or malformed chords in actions. Lints (warnings): a private rule referenced by
nothing; bare `...` alongside `name...` in one block; a `hold` with no `release` on some accepting
path (path-sensitive — checked per rule, with `release(*)` discharging it); prefix-relation notes
naming the completion-timeout cost ("saying \"two\" waits 350ms in case you continue"); adjacent-
slot homophone confusability ("to"/"two", "four"/"for") where both are valid continuations of the
same state. Vocabulary checking walks the rule graph's literal set linearly
(`Grammar::word_set`), so it never needs an expansion.

### Compilation: a word-level transducer

The rule graph compiles into a single NFA whose transitions consume one word and carry **output
ops** (`Emit(fragment)`, `OpenCapture(name)`/`CloseCapture`), with published rules marked on
accepting states alongside their action program (`Press`/`Hold`/`Release`/`ReleaseAll`/`Wait`/
`SpliceAll`/`SpliceCapture(name)` items). A word-level trie is the degenerate case of this automaton.
Determinization is deliberately **not** attempted (the same word can carry different outputs by
context — "red" is shift+F1 as a subject, plain 1 as an assign object): the matcher instead walks
a small **set of alive hypotheses**, each carrying its state, accumulated vector and captures.
Utterances are short and the grammar word-branching is low, so the alive set stays small in
practice; a `MAX_HYPOTHESES` guard turns pathological grammars into a load-time error rather than
a runtime stall.

The matcher's semantics are the original trie machine's, restated over hypothesis sets:
*ambiguous* means "some alive hypothesis is accepting while any hypothesis can consume more
words"; greedy longest-match segmentation, re-sync from the root, `Pending` + completion timeout,
eager partial-driven firing and reconciliation, and confidence gating all carry over — with
command-identity comparisons being comparisons of assembled action vectors, which is what the
n-best gating design specifies. `CommandAction` carries the per-match assembled plan and a
synthesized display name (`Watch(two three, north)`); the executor knows nothing of any of this
apart from `release(*)` (a `ReleaseAll` `KeyEvent` played from the already-tracked pressed-key
set).

### Feeding Vosk

Full per-command expansion is impossible under composition (`squad_selection` alone admits
thousands of subject forms). The recognizer grammar is instead built per **published rule**: rules
whose concrete expansion fits under `MAX_EXPANSIONS_PER_RULE` contribute whole phrases (best
recognition — Vosk sees complete utterances); larger rules are **decomposed at referenced-rule
boundaries** into fragment phrase lists (each private rule's own expansions), relying on the
verified fact that Vosk chains grammar entries within one utterance — which is already how
multi-command utterances reach the matcher today. The matcher's automaton, not Vosk, enforces
which fragment sequences form a real command; invalid orderings decode as clean words and are
dropped by the existing re-sync path. `"[unk]"` is always included. The decomposition is
deterministic and reported by `validate` (which rules were decomposed and why), since it trades
recognition accuracy for feasibility.

### Profile schema v2

```yaml
name: Arma 3
model: ~/.local/share/vosk/vosk-model-en-us-0.22-lgraph
hotkey: { device: auto, key: leftctrl, mode: push-to-talk }
completion_timeout: 350ms
defaults: { duration: 30ms, interval: 30ms }   # press pacing, as before
grammar: |
  Map = "map" | "toggle map" { m }
  ...
```

`grammar:` is an **inline block** — profiles stay single, URL-shareable files. The `commands:`
list, its `phrase:`/`keys:`/`events:` forms and the original phrase DSL are gone. The grammar
parses during profile deserialization (the parse-during-load rule is unchanged: a bad grammar is a
config-load error with an ariadne-rendered excerpt), while the automaton compiles in the command
assembly (`run`/`test`/`validate`), where its errors can name the profile's source.
`voice-orders new` scaffolds a commented grammar; `validate` runs the static analysis above plus
vocabulary checks — see [`validate`](#validate).

### Work plan

| # | Branch | Contents | Status |
|---|---|---|---|
| G1 | `grammar-v2-design` | This design section; `profiles/arma3.yaml`. | done |
| G2 | `grammar-v2-core` | Token/lexer/parser/AST/diagnostics (chumsky + ariadne), static analysis, rule-graph word set. | done |
| G3 | `grammar-v2-output` | `KeyEvent::ReleaseAll` + executor handling; assembly pacing helper (flatten + duration/interval/wait rules). | done |
| G4 | `grammar-v2-automaton` | NFA transducer compiler, accepting-state action programs, expansion/decomposition for the Vosk feed, automaton-level duplicate detection. | done |
| G5 | `grammar-v2-matcher` | Multi-hypothesis matcher walk; pending/eager/confidence gating over hypothesis sets; assembled `CommandAction`s. | done |
| G6 | `grammar-v2-profile` | Profile schema v2, wiring in `run`/`test`/`validate`/`new`, migrated `profiles/*.yaml` + `examples/profile.yaml`, deleted the original DSL and flattened `grammar/v2/` → `grammar/`. | done |
| G7 | `grammar-v2-docs` | VuePress grammar reference rewrite. | not started |

## Matcher: the hypothesis walk + completion timeout

### The walk

The matcher engine (`matcher/engine.rs`) walks utterances over the grammar's compiled
[automaton](#compilation-a-word-level-transducer) with a `Walk` — a set of alive hypotheses, each
carrying its state and accumulated output. Between words the engine asks the walk what the words
so far mean:

- `accepts()` — the readings which form a complete published command right now, each with its
  evaluated action program and display name (an `Accept`);
- `can_extend()` — whether any reading could consume another word;
- `is_ambiguous()` — some reading accepts **and** some can extend, which is the
  completion-timeout condition. Ambiguity is a property of the *walk position*, regardless of
  which commands are involved — a word-level trie's terminal-with-children node, generalized.

Duplicate commands (the same word sequence, different assembled outputs) are detected when the
automaton compiles, before any walk runs — an error in `run`, `test` and `validate` alike.

### The completion-timeout state machine

The matcher consumes `RecognitionEvent`s and produces `CommandAction`s onto the command queue:

```rust
pub struct CommandAction {
    pub command: String,             // display name: rule + captures, e.g. Watch(two, east)
    pub output: CompiledOutput,      // assembled from the action program at fire time
}

enum MatchState {
    Idle,
    Pending {
        accept: Accept,              // matched and ready to fire
        walk: Walk,                  // the resting walk, to continue from
        deadline: tokio::time::Instant,   // now + profile.completion_timeout
    },
}
```

The matcher runs under a `MatcherOptions` struct (completion timeout, the eager switch and its
delay, the confidence margin, and a warning sink) threaded in from the profile by the `run`
assembly. With **eager matching off** (`recognition.eager: false` — the compatibility escape
hatch), commands fire on **Final** results only; partials are used solely to hold a pending timer
open, never to fire. Transitions:

- **Idle + Final(words):** strip `[unk]` tokens, then walk the words from the root with greedy
  longest-match segmentation — when a word kills the walk but the path passed an accept, emit that
  accept's command and re-sync the remaining words from the root; with no accept on the path,
  drop up to the current word and re-sync (logged at debug). At the end of the words:
  - resting on an **unambiguous accept** → fire immediately, go Idle;
  - resting on an **ambiguous accept** → go `Pending { accept, walk, deadline: now + timeout }`;
  - resting mid-phrase with no accept → incomplete phrase, drop, go Idle.
- **Pending + timer fires:** fire the pending command, go Idle.
- **Pending + Final(words):** continue from the pending `walk`. If the continuation reaches a longer
  accept, the longer command *supersedes* the pending one (only the longer fires). If the first
  word does not extend the walk, fire the pending command first, then match the new words from the
  root.
- **Pending + Partial(text):** if the partial's next word extends the pending walk (probed on a
  fork), push the deadline out to `now + timeout` — the speaker is mid-way through the longer
  phrase; don't fire the short command under them. Otherwise ignore.
- **Any + Muted:** clear everything, including a pending command — a half-confirmed command must not
  fire when listening resumes.

### Eager matching (partial-driven firing)

With **eager matching on** (the default), the same walk also runs over every *partial* hypothesis,
and an utterance-scoped `EagerContext` tracks what may fire before the endpointer ever finalizes:

```rust
struct EagerContext {                       // Some(_) from an utterance's first partial to its Final
    origin: Walk,                           // walk origin: the pending walk, or a fresh root walk
    passed: Option<Accept>,                 // pending command absorbed from the previous utterance
    fired: Vec<Match>,                      // (position, accept) already fired from partials
    resting: Option<EagerResting>,          // the armed deadline, if the walk rests on an accept
}
```

- **Certain fires.** A command the greedy walk has *passed and resynced beyond* ended strictly
  before the partial's last word — no revision of the words still being spoken can take it back —
  so it fires **immediately**, recorded in `fired`.
- **Resting on an unambiguous accept** arms `now + eager_delay`, re-armed on every changed
  partial: the hypothesis must hold still before it is trusted. The deadline firing fires the
  command, records it, and keeps the context open.
- **Resting on an ambiguous accept** arms the **completion timeout from the partial** — the wait
  no longer starts at finalization, which is the big win for prefix commands. A later partial which
  does not move the resting point keeps the armed deadline.
- **Resting mid-phrase** (including past an uncommitted crossed accept) arms nothing: the trailing
  words may still grow into the longer phrase.
- **Final(utterance):** *reconciliation*. The full walk runs as usual and its `(position, command)`
  sequence is compared against `fired`: a matching prefix means the remainder fires and an
  ambiguous rest goes `Pending` (keeping the **earlier** partial-armed deadline when it is the same
  wait); `fired` containing exactly the resting command as its one extra entry means the ambiguous
  choice was already made mid-utterance (nothing fires twice, nothing is held pending); anything
  else is an **eager mismatch** — the keys are already down, nothing can be un-pressed, so the rest
  of the utterance is dropped and a warning is emitted through the session's event sink
  (`warning:` line / yellow TUI entry). The context is cleared at every Final.
- **A new utterance's partial while a command is Pending** absorbs the pending command into the
  context (`origin`/`passed`) and disarms its timer: the continuation logic itself now decides its
  fate — an extending hypothesis supersedes it, a non-extending one flushes it immediately. This
  subsumes the eager-off rule where an extending partial merely pushed the deadline.
- **Muted / cancellation** clear the context without firing, exactly as they clear `Pending`. The
  events channel closing fires only what a Final confirmed: a still-undecided absorbed pending
  command, never a bare partial hypothesis.

### Confidence gating (alternatives)

When `recognition.alternatives > 0`, finalized utterances carry Vosk's n-best list. Confidences are
**unnormalized** path scores (measured: ~150–240 for five-word utterances; homophones return
byte-identical scores; acoustically distinct competitors gap by a few units), so only the *margin*
between alternatives of one utterance is meaningful. At each Final, every alternative's text is
resolved through the automaton (from the same walk origin the utterance itself will use) to the
assembled key plans it would run; if any alternative within `confidence_margin` of the 1-best resolves to a
**different non-empty** sequence, the whole utterance is suppressed and a warning names both
readings (`ambiguous: "mortar sentry" vs "rocket sentry" (margin 1.2)`). Alternatives resolving to
the same sequence (homophone phrases of one command) or to nothing are ignored. A suppressed
utterance leaves the matcher Idle; a command left Pending by a *previous* utterance follows the
existing non-extending rule — it was confirmed, so it flushes rather than being dropped or
superseded by an utterance we refused to trust.

Because alternatives exist only on finalized results, confidence gating and eager firing are
per-utterance incompatible: `eager: true` + `alternatives > 0` is a config error, and `alternatives`
flips the eager default to `false`.

The loop is a single `select!`:

```rust
loop {
    tokio::select! {
        _ = cancel.cancelled() => break,
        _ = sleep_until_deadline(&state) => fire_pending(&mut state, &queue).await,
        ev = events.recv() => match ev {
            Some(ev) => transition(&mut state, ev, &automaton, &queue).await,
            None => break,
        },
    }
}
// sleep_until_deadline: sleep_until(deadline) when Pending, std::future::pending() when Idle
```

### Endpointing and latency

Where the latency actually lives, measured on real recorded speech (lgraph model, this codebase):
commands used to fire only on Vosk `Finalized` results, which the endpointer emits after trailing
silence — **700–1000 ms after the partial hypothesis had already stabilized on the exact final
text**. That dead time was the whole user-felt latency, and it also meant `completion_timeout`
started far later than it needed to for ambiguous prefixes. Three independent levers now attack it,
all under the profile's `recognition:` block:

1. **The endpointer itself** (`recognition.silence`, default 200ms). libvosk 0.3.45 exports
   `vosk_recognizer_set_endpointer_delays(rec, t_start_max, t_end, t_max)` and
   `vosk_recognizer_set_endpointer_mode` (verified via `nm`; no published crate binds them — our
   dlopen table in `recognition/libvosk.rs` does). Setting `t_end` = 0.15 s cut the measured
   finalize delay from ~700 ms to ~400 ms with unchanged transcripts; the default ships a
   still-conservative 200 ms (Vosk's own default is ~500 ms). The other two parameters keep the
   values vosk-api's header suggests — `t_start_max` 5.0 s (initial-silence timeout) and `t_max`
   30 s (hard utterance cap) — see the constants in `recognition/vosk.rs`.
2. **Eager (partial-driven) firing** (`recognition.eager`, default on; `recognition.eager_delay`,
   default 100ms). The matcher fires from *stable partials* instead of waiting for finalization at
   all: certain (resynced-past) commands fire instantly, unambiguous resting matches fire after
   `eager_delay` of hypothesis stability, and ambiguous resting matches start their
   `completion_timeout` at the partial rather than at the Final. The eventual Final is reconciled
   against what already fired, and a mismatch is warned about (keys cannot be un-pressed) — see
   §"Eager matching" above. `eager: false` restores the original Final-only machine exactly.
3. **Confidence gating** (`recognition.alternatives` + `recognition.confidence_margin`), the
   opposite trade: spend the finalization wait buying *certainty*, suppressing utterances whose
   close n-best competitors would run different commands — see §"Confidence gating" above.
   Measured: homophones (one/won) return byte-identical unnormalized confidences, distinct
   competitors gap by a few units (4.9 observed), so only the margin is meaningful and gating keys
   off it.

Two honest consequences remain:

1. "autocannon sentry" spoken in one breath is one hypothesis: with eager on it fires
   `eager_delay` after the last word stabilizes; with eager off it fires when the endpointer does
   (`silence` after the last word, plus decode).
2. The completion timeout still only costs you when you *pause* inside an ambiguous phrase — but
   with eager on the short command's perceived latency is now just `completion_timeout` from the
   pause, instead of `endpoint silence + completion_timeout` from the eventual Final.

The remaining risk is inherent: an eager fire acts on a hypothesis, and a speaker who pauses past
`completion_timeout` mid-phrase (or a partial the recognizer later revises) produces keys that
cannot be taken back. The matcher never attempts compensation; it reports the mismatch through the
session's event path so the user sees exactly what fired.

## Profile schema

Profiles are standalone YAML files. serde does all structural validation at load (the grammar
parses and analyzes during deserialization, key names resolve during deserialization, durations
parse via humantime); explicit `validate_*()` methods cover cross-field invariants. The automaton
compiles in the command assembly rather than at load, so its errors can name the profile's source.

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

recognition:               # the latency levers (all optional; block absent = these defaults)
  silence: 200ms           # endpointer trailing silence (t_end); Vosk's own default is ~500ms
  eager: true              # fire from stable partials (default true; false = Final-only firing)
  eager_delay: 100ms       # how long a partial must hold still before an unambiguous match fires
  alternatives: 0          # >0 requests an n-best list and enables confidence gating
  confidence_margin: 3.0   # suppress when a different-command alternative is this close to the winner

defaults:                  # pacing for assembled key plans
  duration: 30ms           # how long each press is held down
  interval: 25ms           # gap between consecutive presses

# TitleCase rules are published as speakable commands; lowercase rules are
# private building blocks (see "Composable command grammars" above).
grammar: |
  Autocannon = "deploy"? "the"? ("autocannon" | "auto cannon") "sentry"? { 4 }
  Terminal = "open" "the"? "terminal" { leftctrl+leftalt+t }
  Salute = "salute" { hold(x), wait(750ms), release(x) }
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

### Output plans

What a command presses is written in its grammar rule's action block (see
[Command semantics](#command-semantics)); the matched program is assembled into a flat event plan
at fire time:

```rust
// output/mod.rs
pub enum CompiledOutput {
    Keyboard(Vec<KeyEvent>),
    // future: Mouse(..), Exec(..), Audio(..)
}

pub enum KeyEvent { Down(KeyCode), Up(KeyCode), ReleaseAll, Wait(std::time::Duration) }
```

`output/assembly.rs` applies the profile's `defaults` pacing: a press puts every key of its chord
`Down` in written order, holds it for `duration`, lifts in reverse order, and consecutive presses
are separated by `interval` (an explicit `wait(..)` *replaces* that interval); `hold`/`release`
carry no implicit pacing, and `release(*)` becomes `ReleaseAll`, played from the executor's
tracked pressed-key set. A `hold` with no `release` on some accepting path is legal (hold-style
macros) but linted at load.

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
    /// Update voice-orders itself to the latest release from GitHub.
    /// See "Self-update" below.
    Update {
        /// The version to move to, as a tag (`v1.2.3`) or bare (`1.2.3`).
        /// Defaults to the latest release newer than this one; an older
        /// version is a rollback.
        version: Option<String>,
        /// Print the available releases instead of installing one.
        #[arg(long)]
        list: bool,
        /// Consider pre-release versions as well as stable ones.
        #[arg(long)]
        prerelease: bool,
        /// Serialized update state, set by the updater when it relaunches us
        /// between phases.
        #[arg(long, hide = true)]
        state: Option<String>,
    },
}
```

`new` writes a scaffold with every option present-but-commented plus a small worked grammar (two
published rules, a private rule and a capture), and refuses to overwrite an existing file (user
error with advice).

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

### The session terminal UI (ratatui)

**Both** `voice-orders test` and `voice-orders run` render as a full-screen terminal UI (ratatui +
crossterm) rather than a line printer when stdout is an interactive terminal, keeping the loop
glanceable mid-game-setup — and, under `run`, mid-game. The machinery is shared (`commands/ui/`:
the event stream, the bounded log, the widget tree, the plan rendering); the two commands differ
only in what they put into the stream. Layout, kept deliberately simple:

- **Header** — the active profile name (left-aligned) with its stats (command count, phrase count,
  hotkey mode) and the profile source (the resolved file path or https URL). Under `run` with a
  wrapped application, the source line also carries `wrapping: <app> (pid …)` on the right.
- **Body** — a scrolling event log where each line leads with a colored severity dot.
- **Footer** — the live listening state, and the loaded model name right-aligned.

`q`/`Ctrl-C` exits. When stdout is not a TTY — piped output, CI, and (crucially) a Steam launch —
both commands fall back to their existing line-oriented behavior, so the wrapper contract and every
script reading `test`'s output are untouched.

**One entry per recognition.** The log is not a transcript of the event stream. A finalized
utterance is appended as one grey entry (`"auto cannon sentry"`), and the match which resolves it
**upgrades that same entry in place** to green with the command and its key plan
(`"auto cannon sentry" → Autocannon sentry (4)`); a greedy multi-match appends its extra commands to
the same line. An entry which never upgrades stays grey — that, rather than an absent line, is the
"it heard me but nothing fired" signal. Interrupted and discarded commands keep their own yellow
entries, because they say something about a command which already fired rather than about the
utterance. Listening-state changes are **not** logged at all: the footer shows the live state, which
is both fewer lines and more current.

Matches are correlated to utterances by order alone: a match belongs to the **oldest** recognition
nothing has matched yet, because the completion timeout means a later utterance can be logged before
an earlier one's match fires. The cost is pinned by a test — an utterance which matches nothing,
followed by one which matches, resolves the wrong way round. Telling those two sequences apart would
need the matcher to say which utterance a command came from, which the event stream does not carry.

**Failures are visible.** A `DecodingState::Failed` from the recognizer becomes
`RecognitionEvent::Failed`, coalesced to one event per run of failures (a decoder which cannot decode
fails on every frame, ~50/s) and re-armed by the next successful decode. The matcher ignores it — it
carries no words, so it must not disturb a pending command — while the UI logs it as a yellow
`warning:` entry and plain `test` prints the same line. Without it, a session where nothing works
looks exactly like one where nobody spoke.

**Child processes under a UI.** Two things change for a wrapped application when a TUI owns the
screen, and both are consequences of the terminal, not of preference:

- **stdio is piped, not inherited.** An inherited child writes straight over the alternate screen.
  Under the UI, stdout and stderr are read line by line and logged as dim white entries prefixed with
  the program name; in plain mode stdio is inherited exactly as before.
- **Ctrl-C is a keystroke, not a signal.** Raw mode is precisely the state in which the terminal
  stops turning Ctrl-C into a SIGINT, so the child never receives one. `q`/`Ctrl-C` therefore takes
  the graceful path by hand: the key handler cancels the UI token, the supervisor turns that into a
  `Shutdown::ForwardSigint` intent (a pure function of "how were we stopped" and "is there a child",
  so it is testable without signalling the test runner), forwards SIGINT with `libc::kill`, and waits
  out the same grace period a SIGTERM gets before shutting the pipeline down. A real SIGINT still
  forwards nothing — the kernel already delivered it to the process group — and a real SIGTERM
  (Steam) behaves exactly as it always has.

Exit codes are unchanged either way: the child's code propagates, and a session the user ended is a
successful one. The terminal is restored *before* anything is printed, so a final human error is
never swallowed by the alternate screen — and `main.rs` leaves the tracing console layer out
entirely when a TUI is going to own stdout.

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
2. Compile the grammar's automaton and build the recognition feed — duplicate commands and
   state-cap violations are errors here, naming the profile's source.
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
6. If `app` is non-empty, spawn it with `tokio::process::Command`: **stdio inherited** in plain mode
   (the wrapper contract, and what a Steam launch always gets), **piped** under the terminal UI,
   whose log the child's output is read into — see §"The session terminal UI (ratatui)".
7. Supervise:

```rust
tokio::select! {
    status = child.wait(), if child.is_some() => { cancel.cancel(); exit_code = status.code().unwrap_or(1); }
    shutdown = interrupt => {                                 // a real SIGINT: the child shares our
        cancel.cancel();                                      // process group, so the kernel already
        if shutdown == Shutdown::ForwardSigint {              // delivered it — but a Ctrl-C read as a
            forward_signal(&child, SIGINT);                   // *keystroke* under the UI reached
        }                                                     // nobody, and has to be sent by hand
    }
    _ = sigterm.recv() => { cancel.cancel(); forward_signal(&child, SIGTERM); }  // Steam shutdown:
                                                              // libc::kill, then a 5s grace before
                                                              // exiting anyway
}
```

Under the terminal UI a **command reporter** sits between the matcher and the executor, reporting
each match to the log on its way through. The executor is deliberately left knowing nothing about
any UI: it is the one part of the pipeline which must never be slowed down or complicated by
reporting, and being *in* the path rather than tapping it means what the log says fired is exactly
what was played, in order — the same arrangement (and the same reasoning) as the recognition
narrator in front of the matcher.

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

Pipeline: load the profile (serde already validated structure and parsed the grammar) → the
grammar's load-time **lints** (reported as `warning:` lines) → the automaton's **compile
diagnostics** (duplicate commands, the state cap — errors, each rendered with its ariadne source
excerpt) → the **vocabulary check** over `Grammar::word_set` → the behavioural **notes**. Output is
one section per published rule plus a profile-wide list; failures render as `human_errors::pretty`
user errors. All problems are reported in a single pass; the exit code is 1 iff any error occurred.

### Vocabulary checking

Implemented against the `Vocabulary` trait (production: `Model::find_word`; tests: a HashSet-backed
fake). For each unique word from `word_set` (linear — no expansion blow-up), check membership. For
misses, suggest in order:

1. **Normalization** — lowercase and strip punctuation (`.,!?'"`); if the normalized form is known,
   suggest it. (Literals already lowercase; this catches punctuation pasted into a grammar.)
2. **Compound decomposition** — for every split point `i`, if both `word[..i]` and `word[i..]` are in
   the vocabulary, suggest the split (e.g. `autocannon` → `"auto cannon"`). Report at most two
   splits, most-balanced first.
3. **Nearest words** — if the model ships a readable word list (`<model>/graph/words.txt`), rank
   candidates by `strsim` Levenshtein distance (≤ 2, preferring a shared first letter) and report the
   top three. Models without a word list get a note that nearest-word suggestions are unavailable.

### Warnings and notes

- **Grammar lints** from static analysis (an unreferenced private rule, a `hold` with no `release`
  on some accepting path, `...` alongside a `name...` splice) surface as warnings; they never fail
  validation on their own.
- **Automaton size:** each published rule gets a `note: compiles into N automaton states`, so a
  repetition at the cap is visible per rule.
- **Decompositions:** each rule the recognition feed could not expand whole gets a note naming its
  concrete expansion count and saying it is decomposed into fragment phrases for recognition (see
  [Feeding Vosk](#feeding-vosk)) — the accuracy trade should be discoverable, not silent.
- **Prefix relations:** a bounded breadth-first sweep of the automaton finds the points where a
  command is complete while a longer one is still possible, and reports one witness per rule:
  `note: saying "autocannon" will wait 350ms in case you continue with "autocannon sentry"` —
  making the completion-timeout behavior discoverable per profile. (Bounded like duplicate
  detection: shortest phrases are swept first, and a grammar too large to sweep exhaustively is
  covered to the budget's depth.)

## Self-update

`voice-orders update` replaces this binary with a release from GitHub, using
[update-rs](https://crates.io/crates/update-rs) — the crate extracted from Git-Tool, whose
three-phase *prepare → replace → cleanup* dance is what lets a running executable overwrite itself.
`src/update.rs` configures it and owns the policy; `src/commands/update.rs` is the user-facing half.

**The manager.** A `GitHubSource` for `SierraSoftworks/voice-rs` with a `v` release-tag prefix, and a
`Launcher` whose resume arguments are `["update", "--state", <json>]` — a sub-command rather than
update-rs' default `RESUME_FLAG`, because voice-orders parses its arguments with clap-derive and a
bare flag would have to be intercepted before `Args::parse()`. There is nothing to carry across the
relaunch, so no extra environment variables. Failures need no conversion at the boundary:
`update_rs::Error` *is* `human_errors::Error`, which is `crate::Error`, so an updater failure already
arrives with a description and advice.

**Asset naming.** The release workflow stages each build as `voice-orders-{os}-{arch}`
(`voice-orders-linux-amd64`, `voice-orders-linux-arm64`) — exactly `update_rs::naming::go`'s Go-style
convention, so no custom pattern is needed. update-rs anchors the glob at both ends, which is what
keeps the `libvosk-linux-<arch>.so` asset published beside it out of the way: **an update replaces
the voice-orders binary and never touches libvosk**, which is the right split, since the two move on
completely independent schedules (see "libvosk & model distribution" below). The expected name is
pinned against a literal of what the workflow produces, so renaming an asset breaks a test rather
than everybody's updater.

**Selection is a pure function.** `update::choose(releases, installed, target, prerelease)` returns
`Install` / `UpToDate` / `Unavailable` / `DevelopmentBuild`, which is the whole policy in one
testable place: an explicit version installs exactly that release (a rollback is as legitimate as a
roll-forward), otherwise the newest release which has an asset for this platform and is *strictly*
newer than what is running wins, with pre-releases excluded unless asked for. A debug build reports
`0.0.0-dev` from `version!()`, which compares against nothing — so it is answered with "self-updates
are only available in released builds" rather than a parse error, and without making the request at
all. `--list` still works there; it simply marks nothing as installed.

**The background check, and why plain mode does not run it.** When the terminal UI starts, it spawns
`update::check_for_update()` as a background task; if that finds a release newer than the one
running, the footer gains a dim cyan `⬆ v1.2.3 — voice-orders update` note. Everything about it is
shaped by *when* it runs — at session start, which under `run` is game-launch time:

- **silent on failure.** Every error is swallowed at `debug!` level. A game launch must never stall,
  warn or fail because GitHub is unreachable.
- **bounded.** A 3s total request timeout, and nothing waits for it; the answer is read on the next
  draw, which the UI's 250ms tick guarantees. No extra redraw machinery, and no new `UiEvent` — this
  is not something the *session* reported, it belongs to the footer rather than the log.
- **skipped in a development build**, which has no version to compare.
- **never run in plain mode.** The check lives inside `tui::run`, which is by construction the
  "stdout is a terminal we own" branch of both `test` and `run` — so a piped, CI or **Steam** launch
  never reaches it and never makes a network call. Adding one to the wrapper path would put GitHub
  between a user and their game for no benefit: nobody reads a Steam launch's stdout.

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

## Windows support

voice-orders is being ported to Windows. This section is the standing design for that port; the
phase table below says how much of it exists today.

### The guiding constraint

**Linux is never compromised for Windows.** The Linux behaviour — the evdev search order, the uinput
capability set, the error text, the log lines, the timing — is the reference implementation, and a
Windows change which would alter any of it is the wrong change. In practice that means: portable
code stays portable, platform code sits behind `#[cfg(target_os = "linux")]` / `#[cfg(windows)]` at
the *narrowest* seam that works, and where the two platforms need different code they get two
functions with one name rather than one function with a branch inside it. The Windows CI job runs
`clippy --all-targets -D warnings` on `windows-latest` so a Linux-only assumption cannot be merged.

### The architecture

**Keyboard output: `SendInput` through `windows-sys`.** Each `KeyEvent` becomes an `INPUT` record
with `KEYEVENTF_SCANCODE` — a **scancode**, not a virtual key, because a game reading raw input sees
a scancode where it ignores a synthesized VK, and because a scancode is layout-independent (`w` is
the key above `s` on AZERTY too, which is what a movement macro means). `output/keys.rs` carries the
encoding per row as `WinKey::{Scan, ScanExt, VirtualKey}`, and the one row which cannot be a
scancode at all — `pause`, whose keyboard sequence is `E1 1D 45` — is injected by virtual key.

`wVk` is left at **zero** on every scancode event. Filling it from
`MapVirtualKeyExW(MAPVK_VSC_TO_VK_EX)` was considered and rejected: that translation runs against
*our* keyboard layout, so on an AZERTY machine the `w` scancode would map to `VK_Z` and the record
whose entire purpose is layout-independence would carry a layout-dependent field. It is also
unnecessary — when Windows turns a `KEYEVENTF_SCANCODE` record into a `WM_KEYDOWN` it derives
`wParam` from the scancode using the *receiving* thread's layout, so an application reading virtual
keys already gets a correct one — and leaving the field alone keeps the whole encoding a pure
function of the key table, which is what lets it be unit-tested on Linux.

Every injected event carries `INJECTED_MARKER` (ASCII `"voic"`) in `dwExtraInfo`. The hotkey hook is
system-wide and therefore sees our own output; without the marker, a macro bound to the same key as
the hotkey would toggle listening every time it ran.

`enigo` was considered and rejected. Two reasons, either sufficient: its extended-key handling is an
acknowledged TODO in its own source (`LWIN`/`RWIN`, the numeric-keypad `Enter` and the media keys
are all missing from its extended list, and it works from virtual keys rather than scancodes
throughout), and it depends on `xkbcommon`/`libxdo` on Linux — pulling a display-server dependency
into a project whose entire premise is sitting *below* the display server.

**Hotkeys: `WH_KEYBOARD_LL`, observe-don't-consume.** A low-level keyboard hook is the closest
Windows analogue of reading `/dev/input/event*`: it sees keys system-wide, including inside
fullscreen games. The hook returns `CallNextHookEx` unconditionally — it *observes* the hotkey and
never swallows it, so the key still reaches the game, exactly as an evdev read does. The hook needs
a thread with a message pump (`GetMessageW`), which is a dedicated OS thread rather than a Tokio
task. Raw Input (`WM_INPUT` with `RIDEV_INPUTSINK`) is the future upgrade: it is the only Windows
API which can say *which keyboard* a key came from, which is what `hotkey.device` would need to
mean anything — until then `hotkey.device` stays a Linux concept and `voice-orders devices` says so.

**libvosk: `libloading`, and a frozen DLL.** The raw `dlopen`/`dlsym` binding was replaced by
`libloading`'s OS-specific `Library` types, which gives both platforms typed symbol lookup with no
`transmute` and lets each keep its own loader flags: `RTLD_NOW | RTLD_LOCAL` on unix,
`LOAD_WITH_ALTERED_SEARCH_PATH` on Windows. The Windows flag matters — `libvosk.dll` is a MinGW
build which imports `libstdc++-6.dll`, `libgcc_s_seh-1.dll` and `libwinpthread-1.dll`, and that flag
is what makes the DLL's own directory the first place those are looked for, so the official zip's
contents can simply be unpacked together.

The last published Windows build is **0.3.45**, which predates
`vosk_recognizer_set_endpointer_delays` and `vosk_recognizer_set_endpointer_mode`. Those two entry
points are therefore **optional**: they resolve to `None` rather than failing the load, and the
recognizer emits one warning ("this libvosk build does not support endpointer tuning;
recognition.silence has no effect") and keeps vosk's stock trailing silence. Every other symbol stays
required — a library missing those is not libvosk.

**Child processes: job objects.** Windows has no per-process signals, so the Linux
`libc::kill(SIGINT/SIGTERM)` forwarding has no direct equivalent. `GenerateConsoleCtrlEvent` reaches
a process *group* rather than a process, which means the child has to have been started in one; a
job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is the reliable containment primitive and is
what the wrapper contract (voice-orders exits, the game goes with it) is built on.

The child is therefore spawned with `CREATE_NEW_PROCESS_GROUP` and immediately assigned to a job
object which is set to kill on close. Stopping it is one shared choreography (`ChildStopPlan`) with a
per-platform tail: **ask, wait, and — only on Windows — kill**. The ask is
`GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child_pid)`, and it is honestly best-effort:
`CTRL_C_EVENT` is never sent, because for any non-zero group id it *succeeds and delivers nothing*,
and even `CTRL_BREAK_EVENT` only reaches a console child — a game has no console control handler and
receives neither. After the same `SIGTERM_GRACE` the Linux path uses, `TerminateJobObject` ends the
whole tree.

The new process group has one cost worth stating: a real Ctrl-C at a Windows console no longer
reaches the child, because a process created with `CREATE_NEW_PROCESS_GROUP` starts with Ctrl-C
handling disabled. `interrupts()` still reports `Shutdown::Quiet` there — the supervisor stays
shared, platform-neutral code — so that path ends the same way every other one does: our handles
close as we exit and the job takes the application with us.

**So, plainly: on Windows, stopping voice-orders terminates the wrapped application rather than
signalling it.** There is no polite alternative to offer a windowed process, and the alternative to
the kill is an orphaned game with no wrapper left to stop it. The kill-on-close limit is what makes
this survive the cases we are not present for — Steam's "Stop" button is an arbitrary kill of *us*,
and `CTRL_CLOSE_EVENT` gives us about five seconds before Windows kills us mid-shutdown; in both, our
handles close as we die and the kernel reaps the job. The handle therefore lives in a process-lifetime
`static` rather than being threaded through `supervise`, which stays shared, platform-neutral code;
the decision logic is a pure function over `(Signal, Containment)` and both platforms' plans are
asserted by unit tests on either platform. The Linux plan has no kill step and its behaviour, its
warning text and its timing are unchanged.

**Paths, the clock and the integrity level.** The config file is `%APPDATA%\voice-orders\config.yaml`
and the models directory `%LOCALAPPDATA%\voice-orders\models` — roaming for a few hundred bytes of
settings, local for a 128MB model. Both derivations take the environment as a parameter and the
platform as a *value* (`cfg!(windows)`, not `#[cfg]`), so each platform's answer is compiled and unit
tested on the other, and neither test has to mutate the environment this process shares. The session
log's clock is the same idea: `GetLocalTime` alone would be wrong, because it breaks down *now* while
a log line has to render the moment its entry was recorded — so what Windows is asked for is the
*zone* (`GetLocalTime` minus `GetSystemTime`, rounded to the minute, daylight saving included), which
is then applied to each entry's own timestamp by the same pure formatter the UTC fallback uses. And
`doctor`'s hook check ends with this process' integrity level (`TokenElevation`), because UIPI is
symmetric: an ordinary process can neither type into an elevated window nor see the keys pressed at
one. It is reported, never failed on — running unelevated is the right way to run voice-orders.

**Audio: cpal's WASAPI host, and `!Send`.** cpal works unchanged, but a WASAPI `Stream` is `!Send`
(COM apartment state is per-thread) where the ALSA one is `Send + Sync`. The pipeline future which
owns the capture handle is therefore `!Send` on Windows and must be awaited where it was built — the
assembly already does that, and the Send-ness assertion in `audio/mod.rs` is Linux-only because on
Windows it would be asserting something false.

### Phases

| # | Phase | Contents | Status |
|---|---|---|---|
| W1 | Portability refactor | The only phase which touches Linux code. `keys.rs` gains a Windows column and literal key codes (with a Linux test pinning them against `evdev`); `libvosk.rs` migrates to `libloading` with the two optional endpointer symbols; every Linux-only module is `cfg`-gated behind a platform-neutral seam (`output::PlatformSink`, `hotkey::watch`, `doctor`'s platform checks, `run`'s signal handling); Windows CI job. | **done** |
| W2 | Keyboard output | `output/sendinput.rs`: a real `SendInput` `KeySink` replacing the stub, driven by `keys::to_windows`. Every injected event carries an `INJECTED_MARKER` in `dwExtraInfo` so W3's hook can ignore our own typing; `wVk` is left at zero for scancode events (see below). `doctor` check 2 presses `f24` through the real sink. | **done** |
| W3 | Hotkey | `hotkey/win.rs`: the `WH_KEYBOARD_LL` hook thread, feeding the same `transition`/`ListenMode` logic the Linux task uses. `hotkey.device` warns and proceeds; `doctor` check 3 installs and removes a real hook. | **done** |
| W4 | Child processes | Job-object containment and a graceful stop for the wrapped application, replacing the `send_signal` no-op. `CREATE_NEW_PROCESS_GROUP` at spawn, `CTRL_BREAK_EVENT` as the ask, `TerminateJobObject` as the guarantee, and a pure `ChildStopPlan` so both platforms' choreography is tested from either. | **done** |
| W5 | Polish | Local time in the session log (`GetLocalTime`/`GetSystemTime` for the zone, applied to each entry's own timestamp), `%APPDATA%`/`%LOCALAPPDATA%` paths, the integrity level on `doctor`'s hook check, and docs. | **done** |
| W6 | Release | Windows build matrix row, the `voice-orders-windows-amd64.exe` asset (pinned to `naming::go` by a test) and the `.zip` which carries it beside the four libvosk DLLs, plus a `windows-latest` test job. | **done** |
| W7 | Verification | End-to-end on real Windows hardware, in a real game. | not started |

While the phases were landing, every stub reported the same shape of failure: an ordinary
`human_errors` **user** error saying the feature "is not implemented in this Windows build yet",
raised at the point the Linux code would have reported a missing device, and `voice-orders doctor`
reported each unfinished piece as its own `✗` line — a `doctor` which claimed a clean bill of health
on a build that cannot press keys would be worse than no `doctor` at all. No stubs remain: what W7
has left to establish is not whether the pieces are implemented but whether they behave, on real
hardware, in a real game. Until they have been, the documentation calls the Windows build a beta.

### Rejected compromises

- **No cross-platform input abstraction replacing uinput.** The tempting refactor is a single
  `InputBackend` trait with evdev and Windows implementations behind it. It was rejected: the
  existing `KeySink` seam is already exactly that trait, at the right altitude, and anything larger
  would mean rewriting working Linux code to suit a platform which has not shipped yet.
- **`synchronize()` stays in `KeySink`.** It is a uinput concept (`EV_SYN`/`SYN_REPORT`) and
  `SendInput` needs no flush, so a purist would drop it from the trait and let uinput batch
  internally. That would change the Linux emission sequence, which is pinned by tests and is what a
  game actually sees. The Windows sink implements it as a no-op instead.
- **`hotkey.device` stays Linux-ranked.** Rather than inventing a Windows meaning for it (a window
  title? a HID path?), the option keeps its evdev semantics and Windows says plainly that it has
  none, until Raw Input makes per-device selection real.
- **No second key table.** One table, three columns, one row per key — a Windows-only table would
  drift, and a Windows-only *test* would mean the mapping was only ever checked on the platform
  nobody develops on. The Windows column is pure data, compiled and tested everywhere.

## Testing strategy

- **Grammar:** lexer/parser/AST tables over the chumsky pipeline; every diagnostic asserted by
  substring with its span; static-analysis error and lint tables; automaton walks asserted against
  expected action programs and display names; feed expansion/decomposition tables; the canonical
  Arma grammar loaded, compiled and swept as a fixture shared across the suites.
- **Matcher:** `#[tokio::test(start_paused = true)]` + `tokio::time::advance`, feeding synthetic
  `RecognitionEvent`s through the real task and asserting `CommandAction` order and timing:
  unambiguous immediate fire; ambiguous → timeout fire; ambiguous → superseding continuation;
  partial extends the deadline; non-extending Final flushes then rematches; `Muted` clears pending;
  `[unk]` stripped; multiple commands in one utterance.
- **Executor:** a `KeySink` trait over the uinput wrapper; a fake sink records `(event, instant)`
  under paused time to assert duration/interval timing, plus the pressed-keys-released-on-cancel
  guarantee.
- **Config:** every shipped profile (`examples/profile.yaml`, `profiles/arma3.yaml`,
  `profiles/helldivers2.yaml`) loaded and compiled in unit tests (the docs-can't-drift trick from
  grey and github-backup); a bad grammar inside YAML surfaces its rendered diagnostics as a load
  error; migrated Helldivers commands walk to the exact key plans the old schema compiled.
- **Keys table:** every name round-trips name → code → uinput/evdev; suggestion quality spot checks.
- **Validate:** fake `Vocabulary` covering normalization, compound splits, and nearest-word ranking
  with an injected word list; lint output assertions.
- **Loader:** wiremock for fetch success/404/HTML-body/gist-rewrite; tempfile for path handling.
- **Gated (`pure_tests`):** real-model `find_word` sanity, real grammar construction, end-to-end
  validate against the small English model in CI.

## Risks & open questions

1. **Vosk endpointing** is not configurable through the published crates, but libvosk 0.3.45 does
   export `vosk_recognizer_set_endpointer_delays`/`_mode`, which our own dlopen binding now drives —
   the latency design is documented under [Endpointing](#endpointing-and-latency).
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
- **Grammar** — the rule language with worked examples, ambiguity and the completion timeout, how
  validation suggestions work.
- **Keys** — the key-name reference, generated from `keys::all_names()` so it cannot drift.
