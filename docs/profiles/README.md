# Profiles

A profile is a single, self-contained YAML file describing what voice-orders should listen for and what it should press
when it hears it. Profiles are portable: they can be kept in a repository, shared as a Gist, and loaded straight from an
`https://` URL.

Everything a profile can say is checked when it loads. The grammar is parsed and statically analyzed, key names are
resolved, durations are parsed, and unknown fields are rejected — so a typo is a load-time error with a precise
location, never a command which mysteriously never fires.

## Example

```yaml{2,7-10,12,14-16,20-28}
name: Deep Rock Galactic
model: ~/.local/share/vosk/vosk-model-small-en-us-0.15

audio:
  device: default

hotkey:
  device: auto
  key: rightctrl
  mode: toggle

completion_timeout: 500ms

defaults:
  duration: 30ms
  interval: 25ms

# TitleCase rules are published as speakable commands; lowercase rules are
# private building blocks. `//` comments run to the end of the line.
grammar: |
  // "deploy the autocannon", "auto cannon sentry", ... — `?` marks a word you
  // may leave unsaid, `( | )` groups the alternatives, and the `{ ... }` block
  // says what a match presses.
  Autocannon = "deploy"? "the"? ("autocannon" | "auto cannon") "sentry"? { 4 }

  Terminal = "open" "the"? "terminal" { leftctrl+leftalt+t }

  Salute = "salute" { hold(x), wait(750ms), release(x) }
```

`voice-orders new <path>` writes a profile just like this one, with every option present as a comment and its default
value shown, so the file you start from doubles as a reference.

::: warning Upgrading a profile written for an earlier release
The `commands:` list is **gone**, along with its `phrase:`, `keys:`, `events:`, `duration:` and `interval:` options and
the old `[optional]` / `{alternate, choices}` phrase DSL. Commands are written as [`grammar:`](#grammar) rules instead.
This is a breaking change, and there is no compatibility mode: loading an old profile fails immediately, naming the
field it could not understand.

```
unknown field `commands`, expected one of `name`, `model`, `audio`, `hotkey`,
`completion_timeout`, `recognition`, `defaults`, `grammar` at line 2 column 1
```

The translation is mechanical. Before:

```yaml
commands:
  - name: Deploy the autocannon
    phrase: deploy [the] {autocannon, auto cannon} [sentry]
    keys: ["4"]

  - name: Salute
    phrase: salute
    events:
      - down: x
      - wait: 750ms
      - up: x
```

After:

```yaml
grammar: |
  Autocannon = "deploy"? "the"? ("autocannon" | "auto cannon") "sentry"? { 4 }
  Salute = "salute" { hold(x), wait(750ms), release(x) }
```

Rule by rule: the command's `name:` becomes the rule's TitleCase name, `[optional]` becomes `"word"?`,
`{alternate, choices}` becomes `("either" | "or")`, a `keys:` list becomes a comma-separated action block, and an
`events:` list becomes `hold(..)`, `wait(..)` and `release(..)` in the same block. Per-command `duration:` and
`interval:` overrides have no replacement — [`defaults`](#defaults) applies to every command, and an explicit
`wait(..)` covers the case where one command needed its own spacing. The [grammar reference](../grammar/README.md)
covers everything the rule language can do that the old DSL could not.
:::

## Options

### name
A friendly name for the profile, used in logs and at the top of `validate` reports. Purely cosmetic; a profile without
one is reported as `<unnamed profile>`.

```yaml
name: Deep Rock Galactic
```

### model
The path to the [Vosk model](../guide/installation.md#models) directory used to recognize speech. A leading `~` is
expanded against `$HOME` when the profile loads, so a profile written this way can be shared between machines without
hard-coding anybody's home directory.

```yaml
model: ~/.local/share/vosk/vosk-model-en-us-0.22-lgraph
```

It may also be a bare model **name**, which is resolved inside your [models directory](#models-path) —
`~/.local/share/vosk` unless your system configuration says otherwise:

```yaml
model: vosk-model-en-us-0.22-lgraph
```

A value counts as a name when it contains no `/` and does not start with `~` or `.`; anything else is a path and is used
exactly as written.

A model has to come from *somewhere*, but it need not come from the profile. Three mechanisms are consulted in order,
so that a profile you share with other people does not have to hard-code a path that only exists on your machine:

1. the `--model <path-or-name>` flag on `validate`, `test` and `run`;
2. the profile's `model:` field, with `~` expanded;
3. the `VOSK_MODEL_PATH` environment variable.

If none of the three is set, the error tells you so and lists all three, along with the models directory a name would
have been looked for in. For a published profile, either write the model as a *name* — portable to anyone who keeps
their models in the usual place — or leave `model:` out entirely and let each person set `VOSK_MODEL_PATH` once in
their shell.

The model must be a **dynamic-graph** model — one containing `graph/Gr.fst` and `graph/HCLr.fst`. Grammar-constrained
recognition is not available on static-graph models, and voice-orders refuses to start against one rather than falling
back to free transcription. See [choosing a model](../guide/installation.md#which-model-to-use);
`voice-orders doctor` checks this for you.

### audio
Which microphone to capture from. The whole block is optional; leaving it out defers to your
[system configuration](#system-configuration), and then to your system default.

```yaml
audio:
  device: default
```

#### audio.device <Badge text="default: from your system configuration, else default"/>
`default` uses whatever your system considers its default input device. Anything else is treated as a
**case-insensitive substring of the device name**, which is far more durable than an index — `"USB Microphone"`,
`"Yeti"` and `"headset"` all work.

```yaml
audio:
  device: USB Microphone
```

Resolution order: this field, then [`audio.device`](#audio-device-2) in your system configuration, then `default`. A
shared profile is better off saying nothing here and letting each machine name its own microphone.

If nothing matches, the error lists every input device voice-orders could see, so you can copy a name straight out of
it — and `voice-orders devices` lists them all at any time:

```sh
voice-orders devices --audio
```

```
Audio inputs (audio.device)
  * "HD-Audio Generic" — system default
    "Elgato Wave XLR"

  Copy any part of a name into 'audio.device' to use that microphone; matching ignores case.
```

### hotkey
The global listen hotkey. **Leaving the whole block out means voice-orders listens all the time** — which is a perfectly
reasonable way to run it, and the only way to run it without needing read access to `/dev/input` — *unless* your
[system configuration](#hotkey-2) sets one, in which case that one applies. Every field you do write here wins over the
machine's, one field at a time.

```yaml
hotkey:
  device: auto
  key: rightctrl
  mode: toggle
```

The hotkey is read from evdev, below the display server, so it works in fullscreen games — and, unavoidably, everywhere
else too. See the [privacy note](../guide/permissions.md#a-note-on-privacy) for exactly what is and is not inspected.

#### hotkey.device <Badge text="default: auto"/>
Which input device to watch. Three forms are accepted:

- `auto` — the first device which reports the configured key. This is what you want in almost every case.
- an exact device node, e.g. `/dev/input/event3`.
- anything else — a case-insensitive substring of the device name, matched against devices which report the configured
  key.

```yaml
hotkey:
  device: Keychron
  key: rightctrl
```

::: tip
`/dev/input/eventN` numbers change when hardware is unplugged and replugged, so prefer `auto` or a name substring.
`voice-orders devices --hotkey` lists every device you can read, with its keyboard rank and the one `auto` would pick:

```
Hotkey devices (hotkey.device)
    /dev/input/event2   "Yubico YubiKey OTP+FIDO+CCID" — types (boot-keyboard set only)
  * /dev/input/event3   "ZSA Technology Labs Voyager" — keyboard; 'device: auto' picks this one
    /dev/input/event15  "PC Speaker" — not a keyboard
```
:::

#### hotkey.key <Badge text="required (here or in your system configuration)" type="danger"/>
The key which controls listening, written as one of the [friendly key names](../keys/README.md). This is a single key,
not a chord.

```yaml
hotkey:
  key: rightctrl
```

An unrecognized name is a load error carrying a "did you mean …?" hint.

The key may come from your [system configuration](#hotkey-2) instead, which is how a shared profile ends up with a
hotkey it never mentions. What is not allowed is a profile which writes a `hotkey:` block when no key comes out of the
merge at all: that is a load error naming the missing field, rather than a profile which quietly never listens.

#### hotkey.mode <Badge text="default: toggle"/>
How pressing the key changes the listening state.

| Mode | Behaviour | Starts |
|---|---|---|
| `toggle` | Each **press** flips listening on or off. The matching release does nothing. | muted |
| `push-to-talk` | Listening only while the key is held down. | muted |
| `push-to-mute` | Listening except while the key is held down. | listening |

```yaml
hotkey:
  key: rightctrl
  mode: push-to-talk
```

Modes are written in kebab-case; `push_to_talk` is a load error which lists the valid alternatives. Keyboard
auto-repeat is ignored in every mode, so holding a push-to-talk key never makes listening flap.

Whenever listening turns off, the recognizer is reset and the matcher clears its state — including any command waiting
on the [completion timeout](#completion-timeout). A half-spoken phrase can never leak across a mute boundary and fire
when you unmute.

#### hotkey.interrupt <Badge text="default: false"/>
Whether stopping listening also stops whatever is being **typed**. Muting always stops us *hearing* you; this decides
what happens to the command already on its way out.

```yaml
hotkey:
  key: leftctrl
  mode: push-to-talk
  interrupt: true
```

| Value | The moment listening stops |
|---|---|
| `false` | The command being typed plays out in full, and anything queued behind it still fires. |
| `true` | The command being typed is abandoned where it stands — even part-way through a `wait:` — every key it is holding is released, and every command still queued behind it is thrown away. |

Leave it `false` when your commands are self-contained inputs which are worse half-entered than late: a Helldivers 2
stratagem code is five arrow keys, and four of them do nothing but leave the menu open. Turn it on when a command holds
keys down or takes a noticeable time to type, so that letting go of push-to-talk is a way to say "stop".

`interrupt: true` never leaves a key held down: the executor releases everything it was holding as it abandons the plan,
exactly as it does when voice-orders shuts down.

::: tip
`voice-orders test` honours this too. With `interrupt: true` a rehearsal takes as long to report a command as `run`
would take to type it, and stopping listening part-way through prints `interrupted: "Sprint"` for it, followed by a
`discarded: "…"` line for each command which was waiting behind it.
:::

### completion_timeout <Badge text="default: 500ms"/>
How long a command which is a **prefix** of a longer one waits, in case you are still talking.

With both `Reload = "reload"` and `ReloadWeapon = "reload weapon"` in a profile, saying "reload" and stopping fires the
short command after this long; carrying on with "weapon" fires the longer one instead and cancels the short one.
Durations are written the way you would say them: `300ms`, `1s`, `1s 500ms`. A bare number is a load error —
voice-orders will not guess whether you meant seconds or milliseconds.

```yaml
completion_timeout: 500ms
```

`voice-orders validate` prints a note for every prefix relation it finds in your profile, quoting this value, so you can
see exactly which of your commands pay the wait. The [grammar reference](../grammar/README.md#ambiguity-and-the-completion-timeout)
explains what makes a point in a grammar ambiguous and what the timeout costs you — including the case where a rule
publishes a shared subject, which makes every command built on it ambiguous.

With [eager matching](#recognition-eager) on (the default), this wait starts the moment the recognizer's in-progress
hypothesis comes to rest on the ambiguous phrase — not hundreds of milliseconds later when the utterance finalizes.

::: warning Going below ~500ms is a gamble
The evidence that you carried on arrives well after the words leave your mouth: the recognizer listens in ~100ms
chunks, and a word only shows up in its hypothesis once it has been (mostly) spoken and decoded. Set this much below
500ms and the short command can fire while the longer phrase's words are still on their way — you say "auto cannon
sentry" in one breath and get the autocannon anyway.
:::

### recognition
How quickly — and how cautiously — speech turns into keys. The whole block is optional, and so is every field in it;
leaving it out gives you the defaults shown here:

```yaml
recognition:
  silence: 200ms          # trailing silence before an utterance is finalized
  eager: true             # fire commands from stable in-progress hypotheses
  eager_delay: 100ms      # how long a hypothesis must hold still before it fires
  alternatives: 0         # >0 asks for an n-best list and enables confidence gating
  confidence_margin: 3.0  # how close a competing reading may score before we refuse to guess
```

The [grammar reference](../grammar/README.md#a-note-on-latency) explains how the three mechanisms fit together.

#### recognition.silence <Badge text="default: 200ms"/>
How much silence after you stop speaking makes the recognizer finalize the utterance. This is the floor under every
command's latency when eager firing is off, and the floor under how quickly a *finalized* transcript (and its
[alternatives](#recognition-alternatives)) exists either way. Vosk's own default is around `500ms`; voice-orders ships
`200ms`, which measurably shortens the wait without changing what is recognized. Raise it if your speech has long
natural pauses which are being cut into separate utterances; lower it (with care) if you want finalization sooner.

#### recognition.eager <Badge text="default: true (false when alternatives is set)"/>
Whether commands may fire from **stable in-progress hypotheses**, before the recognizer finalizes the utterance at all.
The recognizer's hypothesis usually settles on your exact final words hundreds of milliseconds before the
[silence](#recognition-silence) endpointer fires — eager matching claims that time back:

- a command the hypothesis has moved *past* (you kept talking and the walk re-synced beyond it) fires immediately;
- a command the hypothesis is resting on fires once it holds still for [`eager_delay`](#recognition-eager-delay);
- an [ambiguous](#completion-timeout) command starts its completion wait at the hypothesis, not at finalization.

The finalized utterance is always checked against what already fired. If they disagree — you were cut off, or the
recognizer revised itself after a command fired — nothing can un-press a key: the session reports a
`warning:` line naming what fired and what was actually said, and drops the rest of the utterance.

A fire is also a commitment: the words it consumed are spent. If you pause past the
[completion timeout](#completion-timeout) mid-phrase and then carry on, the continuation cannot grow those words into
the longer command on top of the keys already pressed — you get the command the pause chose, and a `warning:` line
telling you the trailing words were dropped.

`eager: false` restores the fire-only-on-finalized behaviour exactly, latency included.

`eager: true` cannot be combined with [`alternatives`](#recognition-alternatives) — alternatives only exist on
finalized results, so an eagerly fired command could never be confidence-checked. Setting `alternatives` without
mentioning `eager` simply turns eager firing off.

#### recognition.eager_delay <Badge text="default: 100ms"/>
How long an unambiguous in-progress hypothesis must stay unchanged before it is trusted enough to fire. Shorter is
faster and twitchier; longer gives the recognizer more room to revise itself before keys go down. Only meaningful with
[`eager`](#recognition-eager) on.

#### recognition.alternatives <Badge text="default: 0 (disabled)"/>
How many alternative transcripts to request for each finalized utterance. Anything above `0` enables **confidence
gating**: when a close runner-up reading of the same audio would have run *different* commands, the utterance is
suppressed entirely — firing nothing beats firing a coin-flip — and a `warning:` line names both readings:

```
warning: ambiguous: "mortar sentry" vs "rocket sentry" (margin 1.2)
```

Alternatives which would run the same command (homophones like "one up" / "won up" written as alternatives of one
rule), or which match nothing at all, never suppress anything. `3`–`5` is a sensible range. Implies [`eager: false`](#recognition-eager).

#### recognition.confidence_margin <Badge text="default: 3.0"/>
How close a competing alternative's confidence must be to the winner's before the utterance counts as ambiguous.
Vosk's confidences are unnormalized scores, so only this *gap* means anything: acoustically distinct readings of a
short phrase typically gap by several units, while genuinely confusable ones land within one or two. Raise it to be
more conservative (more suppressions), lower it to be more trusting.

### defaults
The pacing applied to every command's key presses. The block is optional, and so is each field inside it.

```yaml
defaults:
  duration: 30ms
  interval: 25ms
```

#### defaults.duration <Badge text="default: 30ms"/>
How long each chord is held down before it is released.

#### defaults.interval <Badge text="default: 25ms"/>
The gap left between one chord and the next. There is no trailing wait after the last chord, and an explicit
`wait(..)` in an action block **replaces** this interval rather than adding to it — so `1, wait(200ms), 2` is exactly
200ms apart.

There are no per-command overrides: a command which needs its own spacing writes it out with `wait(..)`, and
`hold(..)` / `release(..)` carry no implicit pacing at all. See
[pacing](../grammar/README.md#pacing) in the grammar reference.

### grammar <Badge text="required" type="danger"/>
The commands this profile listens for, written as [grammar rules](../grammar/README.md). It is the one option a
profile cannot leave out — a profile with no grammar has nothing to listen for.

```yaml
grammar: |
  Salute = "salute" { x }
```

`grammar:` is a YAML **block scalar** (`|`), so the whole grammar lives inline and a profile stays a single,
URL-shareable file. Nothing inside it needs quoting or escaping for YAML's sake.

Each rule is a name, a pattern and an optional `{ ... }` action block. **TitleCase rules are published** as speakable
commands; **lowercase rules are private** building blocks other rules refer to, which is what lets forty commands share
the phrase they all start with:

```yaml
grammar: |
  // A private rule: reusable words with the keys they press.
  direction = ( "north" { up }
              | "south" { down }
              | "east"  { right }
              | "west"  { left } )

  // "deploy the sentry", "deploy turret", ...
  Deploy = "deploy" "the"? ("sentry" { 4 } | "turret" { 5 })

  // "look north", ... — the capture places the direction's press after the
  // map key and its settle time.
  Look = "look" direction:dir { m, wait(20ms), dir... }
```

The [grammar reference](../grammar/README.md) is the full language: literals, alternation, groups, bounded repetition,
captures and splices, action blocks, and how ambiguity and the completion timeout behave. In short:

| Written | Means |
|---|---|
| `"quoted words"` | What you say. Multi-word literals are fine. |
| `"word"?` | May be left unsaid. |
| `("either" \| "or")` | Exactly one of the branches. |
| `other_rule` | Reuses another rule — its words *and* its presses. |
| `thing[1..4]` | A bounded repetition. |
| `thing:name` | Captures the term's presses for `name...` to place. |
| `{ 4, wait(20ms), leftctrl+t }` | What a match presses. |

The grammar is parsed and statically analyzed **while the profile loads**, so a mistake is reported with the exact
place in your grammar it happened, before anything starts:

```
Error: We don't recognize 'leftctlr' as a key name. Did you mean 'leftctrl'?
   ╭─[ <unknown>:2:36 ]
   │
 2 │ ReloadWeapon = "reload" "weapon" { leftctlr+r }
   │                                    ────┬───
   │                                        ╰───── We don't recognize 'leftctlr' as a key name. Did you mean 'leftctrl'?
───╯
```

Things which are legal but suspicious — a private rule nothing refers to, a `hold` nothing releases, a block which
splices the same presses twice — load fine and are reported by [`validate`](../guide/validation.md) as warnings.

## System configuration

A profile says *what to listen for*. Which microphone, which keyboard and where your models live are facts about **your
machine**, not about the profile — and a profile you publish should not carry them. voice-orders reads them from one
optional file instead:

```
~/.config/voice-orders/config.yaml
```

(or `$XDG_CONFIG_HOME/voice-orders/config.yaml` when that variable is set).

Every field is optional, and so is the file: without it, everything behaves exactly as it always has. It is validated
the way profiles are — unknown fields, unknown key names and unparseable durations are errors naming the file — so a
typo here is never silently ignored.

```yaml
audio:
  device: USB Microphone     # the microphone profiles use when they do not name one

hotkey:                      # the hotkey profiles use, merged field by field with theirs
  device: auto
  key: rightctrl
  mode: push-to-talk
  interrupt: false

models:
  path: ~/.local/share/vosk  # where a profile's `model:` *name* is looked for
```

`voice-orders doctor` prints which file it loaded (or that there is none) above its checks, and every check reports the
**merged** values — the microphone a run would actually open, the hotkey it would actually watch.

### audio.device <Badge text="default: default"/>
The microphone used by any profile which does not set [`audio.device`](#audio-device) itself. Same forms as the profile
option: `default`, or a case-insensitive substring of a device name.

Resolution order: the profile's value, then this one, then `default`.

### hotkey <Badge text="field-level merge"/>
The listen hotkey used by profiles which do not fully specify their own. This is a **field-level** merge: for each of
`device`, `key`, `mode` and `interrupt`, the profile's value wins if it set one, otherwise this file's, otherwise the
built-in default (`auto`, `toggle`, `false`).

A hotkey exists **if and only if a `key` comes out of that merge**, which is what makes shared profiles work: publish a
profile with no `hotkey:` block at all, and each person's own configuration supplies the key their hands expect.

```yaml
# ~/.config/voice-orders/config.yaml
hotkey:
  key: rightctrl
  mode: push-to-talk
```

```yaml
# the shared profile: no hotkey block, so it inherits the one above
name: Helldivers 2
grammar: |
  Reinforce = "reinforce" | "reinforcements" { up, down, right, left, up }
```

Two edges are worth knowing:

- A profile which **writes** a `hotkey:` block but ends up with no key — neither it nor this file names one — is a load
  error naming the missing `key`, rather than a profile which silently never listens.
- This file offering a keyless `hotkey:` block (a device, say, but no key) never activates a hotkey in a profile which
  asked for none. That profile listens continuously, exactly as before.

### models.path <Badge text="default: ~/.local/share/vosk"/>
The directory a profile's [`model:`](#model) is resolved against when it is written as a bare **name** rather than a
path. A leading `~` is expanded when the file loads.

```yaml
models:
  path: /srv/vosk
```

```yaml
# in any profile, on any machine which has that model unpacked in its models directory
model: vosk-model-en-us-0.22-lgraph
```

A value counts as a name when it contains no `/` and does not start with `~` or `.`; anything else is a path and is used
exactly as written. This is the tidiest way to share a profile which still names its model: everyone unpacks the model
into their own models directory, and the profile stays portable.

## Loading a profile from a URL

Both `validate` and `run` accept an `https://` URL wherever they accept a path:

```sh
voice-orders validate https://raw.githubusercontent.com/octocat/profiles/main/drg.yaml
voice-orders run https://gist.github.com/octocat/aa5a315d61ae9438b18d0912c4e075db -- %command%
```

**HTTPS only.** An `http://` URL is refused outright, without a request being made — a profile drives your keyboard, and
that is not something worth taking off an unauthenticated transport. If a profile is only available over plain HTTP,
download it yourself and pass the local path.

**Gist URLs get a `/raw` for free.** Pasting the address bar of a Gist is the obvious thing to do, and it gets you an
HTML page rather than YAML — so a `gist.github.com` URL with no `raw` segment in its path gains one, which GitHub
resolves to the first file of the latest revision. URLs on `gist.githubusercontent.com` and `raw.githubusercontent.com`,
and URLs which already name a `raw` segment, pass through untouched.

If a downloaded body still looks like a web page, that is reported as its own error pointing you at the **Raw** button,
rather than as a confusing YAML parse failure.

**No credentials are sent**, so private Gists and private repositories will not work. A non-success response is
reported with its status code and the URL it came from.

**Nothing is cached.** A `run` at game-launch time should fail loudly rather than silently start with a stale profile.
If your network is unreliable, download the profile to a local file and point at that.

## Validating a profile

```sh
voice-orders validate drg.yaml
```

`validate` reports **everything it finds in one pass**, one section per published rule: grammar lints, the diagnostics
raised when the grammar compiles, every word of your grammar checked against your model's vocabulary, and notes about
how the profile will behave. It exits `1` if anything was an error and `0` when there were only warnings and notes,
which makes it easy to run over a repository of profiles in CI.

```
drg.yaml — Deep Rock Galactic

Autocannon
  note: compiles into 30 automaton states
  note: saying "autocannon" will wait 500ms in case you continue with "autocannon sentry"

Terminal
  note: compiles into 15 automaton states

3 commands checked — 0 errors, 0 warnings.
```

The [validation guide](../guide/validation.md) covers every finding it can report and what to do about each one.

## Rehearsing a profile

`validate` checks what a profile *says*. `test` checks what it *does* — with your voice, your microphone and your
model, but without touching your keyboard:

```sh
voice-orders test drg.yaml
```

It runs the full pipeline — audio capture, recognition, the hotkey, the matcher and the completion-timeout state
machine — but instead of opening `/dev/uinput` it shows what would have happened. In a terminal it renders a
full-screen view: a header with the profile's name, stats and source; a scrolling event log; and a footer with the
live listening state and the loaded model. Press `q` (or `Ctrl-C`) to stop. (`voice-orders run` renders the same
view, with the commands actually being played.)

Each recognition is **one line** in that log, and it upgrades in place. The utterance appears in grey the moment it
is heard, and turns green with the command and its keys when the matcher resolves it:

```
19:04:11 ● "deploy the auto cannon" → Autocannon (4)
19:04:19 ● "two watch north" → Watch(two, north) (f2 3 8 1)
19:04:24 ● "reload the thing"
19:04:29 ● warning: the speech recognizer could not decode the audio
```

A command is named by the rule which matched it, with whatever it [captured](../grammar/README.md#captures) in
parentheses — `Watch(two, north)` — so the log says what you actually said and not merely which rule fired.

A line which **stays grey** is the signal to look for: the model understood you, but no rule in your grammar covers
what you said. The dot carries the same reading as the color — green for a matched command, grey for an utterance nothing
matched, yellow for a warning or for a command interrupted or discarded by
[`hotkey.interrupt`](#hotkey-interrupt), red for pipeline errors. The listening state is not logged: the footer shows
it live as you press and release the hotkey.

When its output is piped — into a file, another tool, or CI — it falls back to plain lines, unchanged, with one line
per event:

```
listening: on
heard: "deploy the auto cannon"
matched: "Autocannon" → 4
heard: "reload"
listening: off
```

There, a `heard:` line with no `matched:` line beneath it is the same signal the grey line is on screen.

Because it emits no input events at all, it is safe to run over a terminal you are reading, and it needs **no uinput
permissions** — so it is the right thing to reach for before you have finished setting your system up, and the right
thing to reach for when a command fires and you are not sure which one. `--model` overrides the model here as it does
everywhere else, and Ctrl-C exits.

This is also the fastest way to answer "is it hearing me at all?". A `heard:` line with no `matched:` line means the
recognizer understood you but no rule in your grammar covers what you said; no `heard:` line at all means the audio or
the listening state is the problem, not the grammar.
