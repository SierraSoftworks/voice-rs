# Getting Started

Welcome to voice-orders. This guide walks you from a fresh machine to a working voice macro: installing the speech
engine, picking a model, writing your first profile, checking it, and running it alongside a game.

::: tip
voice-orders is deliberately Linux-only. It reads hotkeys from evdev (`/dev/input/event*`) and types through uinput
(`/dev/uinput`), both of which sit below the display server — which is exactly why it works identically on X11 and
Wayland, and inside fullscreen games.
:::

## Step #1: Install voice-orders and libvosk

Download the latest release tarball from the [GitHub releases page][releases] and unpack it somewhere on your `PATH`.
The release tarballs bundle `libvosk.so` next to the binary, so there is nothing else to install:

```sh
tar -xzf voice-orders-linux-amd64.tar.gz
sudo install -m 0755 voice-orders /usr/local/bin/
```

If you are building from source instead, you will need `libvosk.so` on your linker path first — `vosk-sys` links it
dynamically and there is no pure-crates.io fallback. The [installation guide](./installation.md) covers both routes in
full.

## Step #2: Download a speech model

voice-orders recognizes speech with a [Vosk][vosk] model, which you download once and point your profile at.

**The model must have a dynamic graph.** Grammar-constrained recognition needs `graph/Gr.fst` and `graph/HCLr.fst`
inside the model directory; a model without them can only transcribe free speech, which is not how voice-orders works.
Pointing voice-orders at such a model fails with a "this model cannot be constrained to a grammar" error rather than
quietly degrading.

| Model | Size | Dynamic graph | Verdict |
|---|---|---|---|
| [`vosk-model-en-us-0.22-lgraph`][lgraph] | ~128 MB | yes | **Recommended.** A much larger vocabulary, so far fewer of your phrase words come back as unknown, and it still ships the word list which powers nearest-word suggestions. |
| [`vosk-model-small-en-us-0.15`][small] | ~40 MB | yes | The lightweight option. Works fine, but its smaller vocabulary rejects more words, and it ships no readable `graph/words.txt`, so `validate` cannot suggest nearby words. |
| `vosk-model-en-us-0.22` (static) | ~1.8 GB | no | Rejected — grammar mode is unavailable, which defeats the design. |

Unpack it wherever you keep your models:

```sh
mkdir -p ~/.local/share/vosk
cd ~/.local/share/vosk
curl -LO https://alphacephei.com/vosk/models/vosk-model-en-us-0.22-lgraph.zip
unzip vosk-model-en-us-0.22-lgraph.zip
```

You can name that path in your profile's [`model:`](../profiles/README.md#model) option, pass it as `--model`, or set
`VOSK_MODEL_PATH` once in your shell and leave it out of your profiles entirely — they are consulted in that order.

`~/.local/share/vosk` is also where voice-orders looks for a model named by **name** rather than by path, so a profile
which says `model: vosk-model-en-us-0.22-lgraph` works on any machine which keeps its models there. See
[system configuration](#step-3-5-tell-voice-orders-about-this-machine) if you keep yours somewhere else.

## Step #3: Sort out permissions

voice-orders needs to read `/dev/input/event*` (to see your listen hotkey) and to write to `/dev/uinput` (to type into
your game). Both are protected, and `setup` configures them for you:

```sh
voice-orders setup
```

It shows exactly what it intends to change — a udev rule, the `uinput` module, and your membership of the `input` group
— asks you to confirm, and runs each step through `sudo`. Then check the result:

```sh
voice-orders doctor
```

`doctor` is read-only, and prints a `✓`/`✗` line per check: the uinput device, whether a virtual keyboard can actually
be created, your group membership, readable input devices, an audio input, and a grammar-capable model.

::: warning
Group membership only takes effect at your **next login**, so expect one `✗` until you have logged out and back in.
`doctor` distinguishes "you are not in the group" from "you are in the group but this session predates the change" and
tells you which.
:::

The [permissions guide](./permissions.md) covers both commands in more detail, what they change if you would rather do
it by hand, and an honest account of what reading `/dev/input` does and does not mean for your privacy.

## Step #3.5: Tell voice-orders about this machine

Optional, and worth five minutes. Which microphone you speak into, which keyboard your listen hotkey lives on, and where
you keep your models are facts about *your machine* — not about any profile — so voice-orders reads them from one file:

```
~/.config/voice-orders/config.yaml
```

Start by asking what this machine actually has:

```sh
voice-orders devices
```

```
Audio inputs (audio.device)
  * "HD-Audio Generic" — system default
    "Elgato Wave XLR"

  Copy any part of a name into 'audio.device' to use that microphone; matching ignores case.

Hotkey devices (hotkey.device)
    /dev/input/event2   "Yubico YubiKey OTP+FIDO+CCID" — types (boot-keyboard set only)
  * /dev/input/event3   "ZSA Technology Labs Voyager" — keyboard; 'device: auto' picks this one
    /dev/input/event15  "PC Speaker" — not a keyboard

  Copy a path, or any part of a name, into 'hotkey.device'; 'auto' picks the best-ranked device which reports your key.
```

Then write down whichever answers you like, all of them optional:

```yaml
audio:
  device: Elgato Wave XLR

hotkey:
  key: rightctrl
  mode: push-to-talk

models:
  path: ~/.local/share/vosk
```

Anything a profile sets wins — the hotkey field by field — so this is a set of defaults, not an override. The payoff is
that your profiles stop describing your hardware: a profile with no `hotkey:` block at all picks up the key above, which
means the profile you share with a friend works on their machine and on yours without either of you editing it. See the
[system configuration reference](../profiles/README.md#system-configuration) for every field.

## Step #4: Write your first profile

`voice-orders new` writes a profile with every option present as a comment, plus one worked command per output form:

```sh
voice-orders new drg.yaml
```

Open it and set `model:` to the model you unpacked, then add the commands you want. A minimal, complete profile looks
like this:

```yaml{2,5-6}
name: Deep Rock Galactic
model: ~/.local/share/vosk/vosk-model-en-us-0.22-lgraph

commands:
  - phrase: deploy [the] {autocannon, auto cannon} [sentry]
    keys: ["4"]
```

Phrases are written in a [small DSL](../grammar/README.md): plain words are required in order, `[optional]` groups may
be left unsaid, and `{alternate, choices}` groups require exactly one of their branches. The keys you may press are
listed in the [key reference](../keys/README.md), and every profile option is documented in the
[profile reference](../profiles/README.md).

## Step #5: Validate it

`validate` is the fastest feedback loop you have. It checks the structure, compiles every phrase, lints the output
plans, and looks up every word of every phrase in the model's vocabulary — reporting **everything it finds in one
pass**:

```sh
voice-orders validate drg.yaml
```

```
drg.yaml — Deep Rock Galactic

deploy [the] {autocannon, auto cannon} [sentry]
  ok

1 command checked — 0 errors, 0 warnings.
```

When a word is not in the model's vocabulary you get an error naming the word, with suggestions: a spelling fix, a
compound split (`autocannon` → `auto cannon`), or the nearest words the model actually knows. The exit code is `1` if
anything was an error, and `0` when there were only warnings and notes — so it drops straight into CI if you keep your
profiles in a repository.

## Step #6: Rehearse it

`validate` checks what your profile says; `test` checks what it does. It runs the whole pipeline — your microphone, the
model, the hotkey, the matcher — but emits **no input events at all** and never opens `/dev/uinput`:

```sh
voice-orders test drg.yaml
```

```
listening: on
heard: deploy the auto cannon
matched: Deploy the autocannon
  down 4, wait 30ms, up 4
listening: off
```

Talk to it. Every utterance it recognizes appears as a `heard:` line, every command it matches appears as a `matched:`
line with the exact key plan it *would* have played, and pressing your hotkey shows the listening state changing. Ctrl-C
exits.

Because it presses nothing, it is safe to run over a terminal you are reading — and because it needs no uinput
permissions, you can rehearse a profile before you have finished setting your system up. It is also the quickest way to
answer "is it hearing me at all?": a `heard:` line with no `matched:` line means your phrases do not cover what you
said, while no `heard:` line at all points at the audio device or the listening state instead.

## Step #7: Run it

```sh
voice-orders run drg.yaml
```

Press your listen hotkey, say a phrase, and watch the keys land. To wrap a game so that voice-orders starts and stops
with it — the form you will use in a Steam launch option — put the application after a `--` separator:

```sh
voice-orders run drg.yaml -- /path/to/game
```

voice-orders exits when the child exits, propagating its exit code, and releases every key it was still holding on the
way out. See the [Steam guide](./steam.md) for the launch-option form and per-game profiles.

[releases]: https://github.com/SierraSoftworks/voice-rs/releases
[vosk]: https://alphacephei.com/vosk/
[lgraph]: https://alphacephei.com/vosk/models
[small]: https://alphacephei.com/vosk/models
