# Installation

voice-orders has two moving parts you need on disk before it will start: the **libvosk** shared library, which does the
speech recognition, and a **speech model**, which is the vocabulary and acoustic data it recognizes against. This page
covers both, plus the two ways of getting the binary itself.

## The binary

### From a release tarball <Badge text="recommended" type="tip"/>

The [release tarballs][releases] bundle `libvosk.so` next to the `voice-orders` binary and are linked with
`-Wl,-rpath,$ORIGIN`, which means the binary looks for the library **in its own directory first**. Keep the two files
together and there is nothing to install and no library path to configure:

```sh
tar -xzf voice-orders-linux-amd64.tar.gz
cd voice-orders-linux-amd64
./voice-orders --version
```

::: warning
That rpath is why the two files travel together. If you copy `voice-orders` to `/usr/local/bin` on its own, it will no
longer find the bundled `libvosk.so` and will fail to start — copy the library alongside it, or install libvosk
system-wide as described below.
:::

To install both properly:

```sh
sudo install -m 0755 voice-orders /usr/local/bin/
sudo install -m 0644 libvosk.so /usr/local/lib/
sudo ldconfig
```

### From source

```sh
git clone https://github.com/SierraSoftworks/voice-rs.git
cd voice-rs
cargo build --release
```

Building needs `libvosk.so` available to the linker, because `vosk-sys` links it dynamically and there is no
crates.io fallback — see the next section.

## libvosk

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
| `vosk-model-en-us-0.22-lgraph` | ~128 MB | yes | **The recommended default.** Its much larger vocabulary means far fewer of your phrase words come back unknown from `validate`, and it ships a readable `graph/words.txt`, so `validate` can suggest the nearest words the model does know. |
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

commands:
  - phrase: salute
    keys: ["x"]
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
