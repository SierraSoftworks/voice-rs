# Profiles

A profile is a single, self-contained YAML file describing what voice-orders should listen for and what it should press
when it hears it. Profiles are portable: they can be kept in a repository, shared as a Gist, and loaded straight from an
`https://` URL.

Everything a profile can say is checked when it loads. Phrases are parsed, key names are resolved, durations are parsed,
and unknown fields are rejected — so a typo is a load-time error with a precise location, never a command which
mysteriously never fires.

## Example

```yaml{2,10-12,14,16-18,21-22}
name: Deep Rock Galactic
model: ~/.local/share/vosk/vosk-model-en-us-0.22-lgraph

audio:
  device: default

hotkey:
  device: auto
  key: rightctrl
  mode: toggle

completion_timeout: 350ms

defaults:
  duration: 30ms
  interval: 25ms

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

`voice-orders new <path>` writes a profile just like this one, with every option present as a comment and its default
value shown, so the file you start from doubles as a reference.

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

### completion_timeout <Badge text="default: 300ms"/>
How long a command whose phrase is a **prefix** of another command's phrase waits, in case you are still talking.

With both `reload` and `reload weapon` in a profile, saying "reload" and stopping fires the short command after this
long; carrying on with "weapon" fires the longer one instead and cancels the short one. Durations are written the way
you would say them: `300ms`, `1s`, `1s 500ms`. A bare number is a load error — voice-orders will not guess whether you
meant seconds or milliseconds.

```yaml
completion_timeout: 350ms
```

`voice-orders validate` prints a note for every prefix relation it finds in your profile, quoting this value, so you can
see exactly which of your commands pay the wait. The [grammar reference](../grammar/README.md#ambiguity-and-the-completion-timeout)
explains what makes a phrase ambiguous and what the timeout costs you.

### defaults
Timing shared by every command which uses the [`keys:`](#keys) shorthand. The block is optional, and so is each field
inside it.

```yaml
defaults:
  duration: 30ms
  interval: 25ms
```

#### defaults.duration <Badge text="default: 30ms"/>
How long each chord is held down before it is released.

#### defaults.interval <Badge text="default: 25ms"/>
The gap left between one chord and the next. There is no trailing wait after the last chord.

Both may be overridden per command with [`duration:`](#duration) and [`interval:`](#interval).

### commands <Badge text="required" type="danger"/>
The commands this profile listens for. At least one is required — a profile with none is a load error, because there
would be nothing to listen for.

```yaml
commands:
  - phrase: salute
    keys: ["x"]
```

Each entry takes the options below.

#### name
A friendly name for this command, used in logs and as the section heading in `validate` reports. When it is omitted,
the command is identified by its phrase source exactly as you wrote it — which is often clear enough that you do not
need a name at all.

```yaml
commands:
  - name: Deploy the autocannon
    phrase: deploy [the] {autocannon, auto cannon} [sentry]
    keys: ["4"]
```

#### phrase <Badge text="required" type="danger"/>
What to listen for, written in the [phrase DSL](../grammar/README.md): plain words are required in order, `[optional]`
groups may be left unsaid, and `{alternate, choices}` groups require exactly one of their branches. The two nest freely.

```yaml
phrase: deploy [the] {autocannon, auto cannon} [sentry]
```

Phrases are parsed while the profile loads, so a syntax error is reported with its line and column before anything
starts:

```
You have an unclosed '[' at line 1, column 8 — every optional group needs a matching ']'.
```

#### keys
The shorthand output form: a list of keys to press in sequence. Each entry is either a single
[key name](../keys/README.md) (`"4"`) or a **chord** whose key names are joined with `+`
(`"leftctrl+leftalt+t"`).

```yaml{3}
commands:
  - phrase: open [the] terminal
    keys: ["leftctrl+leftalt+t"]
```

Each chord compiles to: every key **down** in the order written → hold for [`duration`](#duration) → every key **up** in
reverse order, so modifiers outlive the key they modify. Chords are separated by [`interval`](#interval), with no
trailing wait after the last one. The chord above becomes:

```
down leftctrl, down leftalt, down t, wait 30ms, up t, up leftalt, up leftctrl
```

Mutually exclusive with [`events:`](#events); exactly one of the two is required, and an empty list is a load error
naming the command.

::: tip
Spaces around the `+` are a formatting accident rather than an error, so `"leftctrl + leftshift"` parses the same as
`"leftctrl+leftshift"`.
:::

#### events
The explicit output form: full control over what happens and when. Each entry is a single-key mapping — `down:`, `up:`
or `wait:` — and compiles one-to-one into the emitted plan.

```yaml{4-6}
commands:
  - name: Salute
    phrase: salute
    events:
      - down: x
      - wait: 750ms
      - up: x
```

- `down: <key>` presses a key and leaves it held.
- `up: <key>` releases a key.
- `wait: <duration>` waits before the next step.

A step which tries to do two things at once (`down: x` and `up: x` in the same entry) is a load error, as is a step
which does nothing.

An unmatched `down:` is **legal** — that is exactly how a hold-style macro is written — and `validate` reports it as a
note rather than a problem:

```yaml{4}
commands:
  - phrase: hold forward
    events:
      - down: w
```

An `up:` with no preceding `down:` is a warning instead, because it is almost always a copy-paste slip. Whatever your
macros hold, voice-orders releases every key it is still holding when it shuts down — a voice macro must never leave
`W` pressed in your game.

[`defaults`](#defaults) do not apply to this form: every wait is written out explicitly.

#### duration
Overrides [`defaults.duration`](#defaults-duration) for this command's `keys:` list.

```yaml{4}
commands:
  - phrase: charge the shield
    keys: ["f"]
    duration: 1s
```

#### interval
Overrides [`defaults.interval`](#defaults-interval) for this command's `keys:` list.

```yaml{4}
commands:
  - phrase: cycle weapons
    keys: ["1", "2", "3"]
    interval: 100ms
```

Both overrides are ignored by the `events:` form, which has no implicit timing to override.

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
commands:
  - phrase: reinforce
    keys: [up, down, right, left, up]
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

`validate` reports **everything it finds in one pass**, one section per command: structural errors, grammar lints,
output lints, and every word of every phrase checked against your model's vocabulary. It exits `1` if anything was an
error and `0` when there were only warnings and notes, which makes it easy to run over a repository of profiles in CI.

The checks it runs:

| Finding | Severity | Meaning |
|---|---|---|
| A word your model does not know | error | The command can never be recognized. Comes with spelling, compound-split and nearest-word suggestions. |
| Every term in a phrase is optional | error | The phrase expands to include the empty phrase, which nobody can say. |
| A phrase expanding past 512 concrete phrases | error | The per-command cap; split the command up. |
| A phrase expanding past 128 concrete phrases | warning | Legal, but the grammar is getting large and the command easy to trigger by accident. |
| The same phrase used by two commands | warning | Only one of them can ever fire. (An error in `run`.) |
| `up:` without a preceding `down:` | warning | The key is released at a keyboard which is not holding it. |
| `down:` with no matching `up:` | note | A hold-style macro. Nothing releases the key until another command does, or voice-orders shuts down. |
| One phrase being a prefix of another | note | Names the wait imposed by [`completion_timeout`](#completion-timeout). |

## Rehearsing a profile

`validate` checks what a profile *says*. `test` checks what it *does* — with your voice, your microphone and your
model, but without touching your keyboard:

```sh
voice-orders test drg.yaml
```

It runs the full pipeline — audio capture, recognition, the hotkey, the matcher and the completion-timeout state
machine — but instead of opening `/dev/uinput` it prints what would have happened:

```
listening: on
heard: deploy the auto cannon
matched: Deploy the autocannon
  down 4, wait 30ms, up 4
heard: reload
matched: Reload  (after waiting 350ms)
  down r, wait 30ms, up r
listening: off
```

Because it emits no input events at all, it is safe to run over a terminal you are reading, and it needs **no uinput
permissions** — so it is the right thing to reach for before you have finished setting your system up, and the right
thing to reach for when a command fires and you are not sure which one. `--model` overrides the model here as it does
everywhere else, and Ctrl-C exits.

This is also the fastest way to answer "is it hearing me at all?". A `heard:` line with no `matched:` line means the
recognizer understood you but no command's phrase covers what you said; no `heard:` line at all means the audio or the
listening state is the problem, not the grammar.
