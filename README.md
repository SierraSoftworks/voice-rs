# voice-orders

**Speak a phrase, press the keys: a Linux-native voice macro tool.**

[![Rust](https://github.com/SierraSoftworks/voice-rs/actions/workflows/rust.yml/badge.svg)](https://github.com/SierraSoftworks/voice-rs/actions/workflows/rust.yml)
[![Documentation](https://github.com/SierraSoftworks/voice-rs/actions/workflows/docs-website.yml/badge.svg)](https://sierrasoftworks.github.io/voice-rs/)
![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)

voice-orders is a voice macro tool for Linux in the spirit of VoiceAttack and LinVAM: you say a command phrase, and it
presses keys in your game for you. It listens only for the phrases your profile defines, it types through the kernel
rather than through the display server, and it gets out of the way when your game exits.

📖 **[Read the documentation →](https://sierrasoftworks.github.io/voice-rs/)**

## Features

- **Grammar-constrained recognition.** Your profile is compiled into a [Vosk][vosk] recognition grammar containing only
  the phrases it can act on, so recognition is a choice between your commands rather than a transcription of English.
- **Kernel-level input.** Hotkeys come from evdev and keystrokes go out through a uinput virtual keyboard, both below
  the display server — so it behaves identically on X11 and Wayland, and works inside fullscreen games.
- **Ambiguity handled for you.** When one command's phrase is a prefix of another's, a configurable completion timeout
  waits to see whether you are still talking instead of firing the wrong macro.
- **A small, expressive phrase DSL.** `deploy [the] {autocannon, auto cannon} [sentry]` covers eight ways of saying the
  same thing in one line.
- **Shareable YAML profiles.** One self-contained file, loadable from disk or straight from an `https://` URL.
- **A drop-in Steam wrapper.** `voice-orders run profile.yaml -- %command%` launches your game, listens while it runs,
  and exits with its exit code.
- **Guided setup and diagnostics.** `voice-orders setup` configures the device permissions; `voice-orders doctor` tells
  you what is still wrong.

## Installation

Download the latest tarball from the [releases page][releases] and unpack it. The tarballs bundle `libvosk.so`
alongside the binary, so there is nothing else to install:

```sh
tar -xzf voice-orders-linux-amd64.tar.gz
./voice-orders --version
```

> **Note:** voice-orders links `libvosk.so` dynamically, and there is no crates.io fallback. The release tarballs
> handle that for you; if you build from source, or install the binary on its own, you will need libvosk on your
> library path. See the [installation guide](https://sierrasoftworks.github.io/voice-rs/guide/installation.html).

You will also need a **dynamic-graph** speech model — `vosk-model-en-us-0.22-lgraph` (~128 MB) is the recommended one:

```sh
mkdir -p ~/.local/share/vosk && cd ~/.local/share/vosk
curl -LO https://alphacephei.com/vosk/models/vosk-model-en-us-0.22-lgraph.zip
unzip vosk-model-en-us-0.22-lgraph.zip
```

## Permissions

voice-orders needs to read `/dev/input/event*` and write to `/dev/uinput`. Run `voice-orders setup` to configure the
udev rule, the `uinput` module and your `input` group membership, then log out and back in and check with
`voice-orders doctor`. See the [permissions guide](https://sierrasoftworks.github.io/voice-rs/guide/permissions.html)
for what it changes, how to do it by hand, and what reading `/dev/input` does and does not mean for your privacy.

## Quickstart

```sh
voice-orders setup             # configure device permissions (once)
voice-orders doctor            # ✓/✗ diagnosis: devices, permissions, microphone, model
voice-orders new drg.yaml      # scaffold a profile with every option documented
$EDITOR drg.yaml               # add your commands
voice-orders validate drg.yaml # check the phrases against your model's vocabulary
voice-orders test drg.yaml     # rehearse out loud — nothing is pressed
voice-orders run drg.yaml      # go
```

## Profiles

```yaml
name: Deep Rock Galactic
model: ~/.local/share/vosk/vosk-model-en-us-0.22-lgraph

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

  - name: Open the terminal
    phrase: open [the] terminal
    keys: ["leftctrl+leftalt+t"]

  - name: Salute
    phrase: salute
    events:
      - down: x
      - wait: 750ms
      - up: x
```

Every option is documented in the [profile reference][profiles], the phrase syntax in the
[grammar reference][grammar], and all 121 key names in the [key reference][keys].

## Steam

Set the game's launch options to:

```
voice-orders run /home/you/profiles/drg.yaml -- %command%
```

Steam substitutes `%command%` with the game (and any Proton wrapper), voice-orders spawns it, listens while it runs,
and exits with its exit code — releasing every key it was still holding on the way out. See the
[Steam guide][steam].

## Documentation

📖 **[sierrasoftworks.github.io/voice-rs](https://sierrasoftworks.github.io/voice-rs/)**

- [Getting started](https://sierrasoftworks.github.io/voice-rs/guide/)
- [Installation][installation] — libvosk and models
- [Permissions][permissions] — udev, the `input` group, and privacy
- [Steam integration][steam]
- [Profile reference][profiles]
- [Grammar reference][grammar]
- [Key reference][keys]

## Contributing

Issues and pull requests are welcome on [GitHub](https://github.com/SierraSoftworks/voice-rs). The design of the tool,
including the reasoning behind most of its trade-offs, is written up in [DESIGN.md](./DESIGN.md).

## License

MIT.

[vosk]: https://alphacephei.com/vosk/
[releases]: https://github.com/SierraSoftworks/voice-rs/releases
[installation]: https://sierrasoftworks.github.io/voice-rs/guide/installation.html
[permissions]: https://sierrasoftworks.github.io/voice-rs/guide/permissions.html
[steam]: https://sierrasoftworks.github.io/voice-rs/guide/steam.html
[profiles]: https://sierrasoftworks.github.io/voice-rs/profiles/
[grammar]: https://sierrasoftworks.github.io/voice-rs/grammar/
[keys]: https://sierrasoftworks.github.io/voice-rs/keys/
