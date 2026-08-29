# Installation

voice-orders has two moving parts you need on disk before it will start: the **libvosk** shared library, which does the
speech recognition, and a **speech model**, which is the vocabulary and acoustic data it recognizes against. This page
covers both, plus the three ways of getting the binary itself.

## The binary

### From the Homebrew tap <Badge text="recommended" type="tip"/>

```sh
brew install sierrasoftworks/tap/voice-orders
```

The formula installs the `voice-orders` binary only, so you still need `libvosk.so` — put it in the Homebrew prefix's
`lib` directory, which the binary's rpath covers:

```sh
curl -fsSL -o "$(brew --prefix)/lib/libvosk.so" \
  https://github.com/SierraSoftworks/voice-rs/releases/latest/download/libvosk-linux-amd64.so
voice-orders --version
```

### From the release binaries

The [releases page][releases] publishes plain, unarchived files — `voice-orders-linux-<arch>` and the matching
`libvosk-linux-<arch>.so` — so you can download exactly the one you want and drop it where you like:

```sh
curl -fsSLO https://github.com/SierraSoftworks/voice-rs/releases/latest/download/voice-orders-linux-amd64
curl -fsSLO https://github.com/SierraSoftworks/voice-rs/releases/latest/download/libvosk-linux-amd64.so
chmod +x voice-orders-linux-amd64
```

The binary carries an rpath of `$ORIGIN`, `$ORIGIN/../lib` and `$ORIGIN/../../../../lib`, which means it will find
`libvosk.so` **beside itself**, in the `lib` directory of a `bin`/`lib` prefix such as `/usr/local`, or in a Homebrew
prefix — and failing all of those, wherever `ldconfig` and `$LD_LIBRARY_PATH` say. Renaming the library to
`libvosk.so` is what matters; the architecture suffix is only there to keep the release assets distinct.

To install both properly:

```sh
sudo install -m 0755 voice-orders-linux-amd64 /usr/local/bin/voice-orders
sudo install -m 0644 libvosk-linux-amd64.so /usr/local/lib/libvosk.so
sudo ldconfig
```

::: tip
voice-orders loads `libvosk.so` on demand rather than linking it, so a machine without it still starts — `--version`,
`setup` and `doctor` all work, and the commands which actually recognize speech (`run`, `test`, `validate`) tell you
what to install instead of failing to launch. `voice-orders doctor` reports the library as one of its checks.

If it is installed somewhere none of those paths reach, point at it directly:

```sh
export VOSK_LIB_PATH=/opt/vosk/libvosk.so   # the library, or the directory holding it
```
:::

### From source

```sh
git clone https://github.com/SierraSoftworks/voice-rs.git
cd voice-rs
cargo build --release
```

Building needs `libvosk.so` available to the linker, because `vosk-sys` links it dynamically and there is no
crates.io fallback — see the next section.

### Updating

A released binary can replace itself:

```sh
voice-orders update           # move to the latest release
voice-orders update --list    # see what is available; * marks the one you have
voice-orders update v1.2.3    # move to a specific version, including backwards
```

It downloads the release asset for your platform, swaps it over the binary you ran, and exits — so
run it from wherever `voice-orders` is installed, and make sure that file is writable by you (a
`/usr/local/bin` install wants `sudo`). Add `--prerelease` if you want release candidates offered
too.

::: tip
`update` replaces the **voice-orders binary only**. `libvosk.so` and your speech model are left
exactly where they are, because they move on their own schedule — a voice-orders release does not
imply a new libvosk. If you ever need a newer library, install it the same way you did the first
time.

A build you made yourself (`cargo build`) will tell you that self-updates are only available in
released builds — there is nothing for it to replace.
:::

When `test` or `run` are showing their full-screen terminal UI, they check for a newer release in the
background and, if there is one, add a dim `⬆ v1.2.3 — voice-orders update` note to the footer. The
check is silent and time-limited, so an unreachable GitHub costs you nothing — and it does **not**
happen when output is piped or redirected, which includes launching through Steam: nothing gets
between you and your game.

## Windows

Windows is packaged as a single zip, because the binary and libvosk have to travel together there:

```
voice-orders-windows-amd64.zip
├── voice-orders.exe
├── libvosk.dll
├── libstdc++-6.dll
├── libgcc_s_seh-1.dll
└── libwinpthread-1.dll
```

Unzip it **anywhere** — a folder in your user profile is fine, and nothing is installed, registered or elevated.
Windows looks for a DLL in the executable's own directory first, so voice-orders finds `libvosk.dll` wherever the
folder happens to be, and `libvosk.dll` finds the three MinGW runtime DLLs beside it the same way. Keep all five files
together and it works; separate them and it will not.

There is also a plain `voice-orders-windows-amd64.exe` asset on the [releases page][releases]. That one is what
`voice-orders update` downloads to replace an installed binary — it carries no DLLs, so it is not the file to start
with.

::: warning
The Windows build of libvosk is frozen at **0.3.45**, which predates Vosk's endpointer controls. Two consequences:

- [`recognition.silence`](../profiles/README.md#recognition-silence) has no effect: Vosk's stock trailing-silence
  behaviour decides when an utterance has ended. voice-orders says so once, at startup — *"this libvosk build does not
  support endpointer tuning; recognition.silence has no effect"* — rather than letting the option quietly do nothing.
- Commands still fire from partial results the moment the match is unambiguous, so what you wait for is eager
  matching's latency and not the full end-of-utterance one. It is the reason the frozen library is liveable.
:::

Permissions need nothing on Windows; see the [permissions guide](./permissions.md#on-windows). Everything below about
models applies unchanged, except that the models directory a bare `model:` name is resolved against is
`%LOCALAPPDATA%\voice-orders\models`, and the configuration file is `%APPDATA%\voice-orders\config.yaml`.

## libvosk

::: tip
Windows users can skip this section — `libvosk.dll` is in the zip above.
:::

Download the prebuilt library from the [Vosk API releases][vosk-releases] — the file is named
`vosk-linux-x86_64-<version>.zip` (or `vosk-linux-aarch64-<version>.zip` on ARM):

```sh
curl -LO https://github.com/alphacep/vosk-api/releases/download/v0.3.45/vosk-linux-x86_64-0.3.45.zip
unzip vosk-linux-x86_64-0.3.45.zip
```

Then either install it system-wide:

```sh
sudo install -m 0644 vosk-linux-x86_64-0.3.45/libvosk.so /usr/local/lib/
sudo install -m 0644 vosk-linux-x86_64-0.3.45/vosk_api.h /usr/local/include/
sudo ldconfig
```

…or leave it where it is and point the toolchain at it, which is the tidier option when you are building:

```sh
export LIBRARY_PATH="$PWD/vosk-linux-x86_64-0.3.45:$LIBRARY_PATH"
export LD_LIBRARY_PATH="$PWD/vosk-linux-x86_64-0.3.45:$LD_LIBRARY_PATH"
```

`LIBRARY_PATH` is what the linker uses at build time; `LD_LIBRARY_PATH` is what the loader uses at run time. You need
both if you are compiling, and only the second if you are just running a binary you already have.

## Models

A model is a directory you unpack once and point your profile's [`model:`](../profiles/README.md#model) option at.

### Which model to use

Grammar-constrained recognition requires a model with a **dynamic graph** — that is, one containing `graph/Gr.fst` and
`graph/HCLr.fst`. Models built with a static graph can only transcribe free speech, and voice-orders will refuse to
start against one with a "this model cannot be constrained to a grammar" error rather than falling back to a mode that
would fire your macros off unrelated chatter.

| Model | Size | Dynamic graph | Notes |
|---|---|---|---|
| `vosk-model-en-us-0.22-lgraph` | ~128 MB | yes | **The recommended default.** Its much larger vocabulary means far fewer of your grammar's words come back unknown from `validate`, and it ships a readable `graph/words.txt`, so `validate` can suggest the nearest words the model does know. |
| `vosk-model-small-en-us-0.15` | ~40 MB | yes | The lightweight option, and a fine choice if download size or memory matters. Its vocabulary is smaller, and it does not ship a readable word list — `validate` will still offer spelling fixes and compound splits, but not nearest-word suggestions. |
| `vosk-model-en-us-0.22` | ~1.8 GB | **no** | Not usable. It is a static-graph model, so it cannot be constrained to a grammar. |

The full list lives at [alphacephei.com/vosk/models][models]; anything described as an "lgraph" or "dynamic graph"
model will work.

### Where to unpack it

Anywhere you like — voice-orders only cares about the path you give it. The convention used throughout these docs, and
in the profile written by `voice-orders new`, is `~/.local/share/vosk/`:

```sh
mkdir -p ~/.local/share/vosk
cd ~/.local/share/vosk
curl -LO https://alphacephei.com/vosk/models/vosk-model-en-us-0.22-lgraph.zip
unzip vosk-model-en-us-0.22-lgraph.zip
rm vosk-model-en-us-0.22-lgraph.zip
```

That leaves you with `~/.local/share/vosk/vosk-model-en-us-0.22-lgraph/`, which is what goes in your profile:

```yaml{2}
name: Deep Rock Galactic
model: ~/.local/share/vosk/vosk-model-en-us-0.22-lgraph

grammar: |
  Salute = "salute" { x }
```

A leading `~` is expanded against `$HOME` when the profile loads, so a profile written this way can be shared between
machines (and between users) without hard-coding anybody's home directory. You can also pass `--model <path>` on the
command line, or set `VOSK_MODEL_PATH` in your shell and leave `model:` out of your profiles entirely — the three are
consulted in that order.

::: tip
Unzip the archive, don't just extract one file out of it. voice-orders needs the whole model directory — if `model:`
points at a directory which is missing `graph/`, `am/` or `conf/`, opening it fails and `validate` reports it as a
finding alongside everything else it checked.
:::

## Next steps

With the binary, the library and a model in place, the remaining first-run hurdle is device permissions:

```sh
voice-orders setup
voice-orders doctor
```

See the [permissions guide](./permissions.md) for what those two do, and for the manual equivalents.

[releases]: https://github.com/SierraSoftworks/voice-rs/releases
[vosk-releases]: https://github.com/alphacep/vosk-api/releases
[models]: https://alphacephei.com/vosk/models
