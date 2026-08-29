---
home: true

actions:
    - text: Get Started
      link: /guide/
    - text: Download
      link: https://github.com/SierraSoftworks/voice-rs/releases
      type: secondary

features:
    - title: Grammar Constrained Accuracy
      details: |
        Your profile is compiled into a Vosk recognition grammar containing only what it can act on, so the
        recognizer never has to guess between "deploy the sentry" and the rest of the English language.

    - title: Works Inside Games
      details: |
        Hotkeys are read from evdev and keystrokes are typed through a uinput virtual keyboard, both of which sit below
        the display server — so voice-orders behaves identically on X11, on Wayland, and in fullscreen.

    - title: Commands Which Compose
      details: |
        Rules build on rules, so forty commands can share one spoken subject — and when a short command is the prefix
        of a longer one, a configurable completion timeout waits to see whether you are still talking.

    - title: Shareable YAML Profiles
      details: |
        A profile is one self-contained YAML file which can be loaded from disk or straight from an https:// URL,
        so sharing a set of commands for a game is as easy as sharing a Gist link.

    - title: A Drop-in Steam Wrapper
      details: |
        `voice-orders run profile.yaml -- %command%` launches your game, listens while it runs, and exits with its exit
        code — which makes it a one-line Steam launch option.
---

voice-orders is a Linux-native voice macro tool in the spirit of VoiceAttack and LinVAM: you speak a command phrase and
it presses keys in your game for you. It listens only for the phrases your profile defines, it types through the kernel
rather than through the display server, and it gets out of the way when your game exits.

## Quickstart

```sh
# Configure the udev rule, the uinput module and your group membership
voice-orders setup

# Check the diagnosis: devices, permissions, microphone, model
voice-orders doctor

# Scaffold a profile with every option documented in comments
voice-orders new drg.yaml

# Edit it, then check the grammar against your model's vocabulary
voice-orders validate drg.yaml

# Rehearse it out loud — no keys are pressed, no uinput needed
voice-orders test drg.yaml

# Listen while a game runs, exiting when it does
voice-orders run drg.yaml -- %command%
```

You will need [libvosk][libvosk] and a speech model on disk before the first run — see the
[installation guide](./guide/installation.md) — and `setup` handles the device permissions, which are explained in full
in the [permissions guide](./guide/permissions.md).

## Example

```yaml
name: Deep Rock Galactic
model: ~/.local/share/vosk/vosk-model-en-us-0.22-lgraph

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
  Autocannon = "deploy"? "the"? ("autocannon" | "auto cannon") "sentry"? { 4 }

  Terminal = "open" "the"? "terminal" { leftctrl+leftalt+t }

  Salute = "salute" { hold(x), wait(750ms), release(x) }
```

Every option in that file is documented in the [profile reference](./profiles/README.md), the rule language is covered
in the [grammar reference](./grammar/README.md), and every key name you may write is listed in the
[key reference](./keys/README.md).

## Releases

`brew install sierrasoftworks/tap/voice-orders` installs the latest release from the
[Homebrew tap](https://github.com/SierraSoftworks/homebrew-tap). Pre-built Linux binaries and the matching
`libvosk.so` are also published as plain files on the
[GitHub releases page](https://github.com/SierraSoftworks/voice-rs/releases).

[libvosk]: https://alphacephei.com/vosk/
