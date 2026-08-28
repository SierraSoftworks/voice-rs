# Permissions

voice-orders talks to two kernel devices, and both of them are protected:

- **`/dev/input/event*`** — where it watches for your listen hotkey. Reading these needs membership of the `input`
  group.
- **`/dev/uinput`** — where it creates the virtual keyboard it types your macros through. Writing this needs the
  `uinput` module to be loaded and a udev rule granting the `input` group access to it.

One group covers both sides, and two subcommands do the work: `voice-orders setup` configures the system, and
`voice-orders doctor` tells you whether it worked. The manual instructions further down are the same steps written out,
for when you would rather do it yourself or need to understand what changed.

The last section of this page is an honest account of what reading `/dev/input` does and does not mean.

## The quick way

```sh
voice-orders setup
voice-orders doctor
```

### `voice-orders setup`

`setup` runs the same checks `doctor` does, works out which pieces are missing, and prints exactly what it intends to
change before it changes anything. Nothing happens until you confirm.

```
voice-orders setup will make the following changes:

  1. create /etc/udev/rules.d/60-voice-orders-uinput.rules
     KERNEL=="uinput", GROUP="input", MODE="0660"
  2. create /etc/modules-load.d/voice-orders.conf containing 'uinput', and load the module now
  3. add you to the 'input' group (usermod -aG input you)
  4. reload the udev rules (udevadm control --reload-rules && udevadm trigger)

Continue? [y/N]
```

Only the missing pieces are listed, so running it a second time after a partial setup does not redo work. When you are
not already root, each step is run through `sudo`, spawned interactively so the password prompt works normally.

Two flags:

| Flag | Effect |
|---|---|
| `--print` | Print the equivalent shell commands and exit **without changing anything**. Useful if you want to read them first, apply them with your configuration manager, or paste them into a machine setup script. |
| `--yes` | Skip the confirmation prompt. For unattended setup only — read what it does first. |

`setup` finishes by reminding you that group membership takes effect at your next login, and suggesting `doctor` to
verify.

### `voice-orders doctor`

`doctor` is **read-only**: it diagnoses, it never changes anything. Each check prints a `✓` or a `✗`, a failing check
explains what to do about it, and the exit code is `1` if any check failed — so it drops into a support conversation or
a setup script equally well.

```sh
voice-orders doctor
```

It checks:

1. **`/dev/uinput` exists** — if not, the `uinput` module is not loaded.
2. **A virtual keyboard can actually be created** — it opens a uinput device and immediately destroys it. This is the
   definitive permissions test; everything else about uinput is inference.
3. **Input access** — that you are in the `input` group, and that at least one `/dev/input/event*` keyboard device is
   actually readable.
4. **An audio input device is present.**
5. **A model resolves and is grammar-capable** — following the usual `--model` → profile `model:` → `VOSK_MODEL_PATH`
   order, and confirming it has a dynamic graph rather than a static one.

Pass a profile and it checks that too — that the profile loads, and that its configured hotkey device resolves to a
real device:

```sh
voice-orders doctor drg.yaml
```

::: tip
The check people trip over is group membership, because it only applies to sessions started *after* the change.
`doctor` distinguishes "you are not in the `input` group" from "you are in the `input` group, but this session started
before that was true — log out and back in", by comparing your effective groups against your configured ones.
:::

## Doing it by hand

This is what `setup` does, written out. `voice-orders setup --print` will print the same commands for you.

### Step #1: Join the `input` group

```sh
sudo usermod -aG input $USER
```

::: warning
**Log out and back in** (or reboot) before trying again. Group membership is attached to your session when you log in,
so a shell you already had open will still be denied, and so will a game launched from a desktop session that started
before you ran the command. `id -nG` should list `input` once it has taken effect.
:::

### Step #2: Load the `uinput` module

`uinput` is a kernel module which most distributions do not load until something asks for it. Load it now, and arrange
for it to be loaded at every boot:

```sh
sudo modprobe uinput
echo uinput | sudo tee /etc/modules-load.d/voice-orders.conf
```

### Step #3: Add the udev rule

By default `/dev/uinput` is owned by `root:root` with mode `0600`, so being in the `input` group is not by itself
enough. This rule hands it to the group:

```sh
echo 'KERNEL=="uinput", GROUP="input", MODE="0660"' | sudo tee /etc/udev/rules.d/60-voice-orders-uinput.rules
sudo udevadm control --reload-rules
sudo udevadm trigger
```

If `/dev/uinput` already existed before you added the rule, unloading and reloading the module is the quickest way to
have it re-created with the new ownership:

```sh
sudo modprobe -r uinput && sudo modprobe uinput
```

### Step #4: Check your work

`voice-orders doctor` is the thorough answer, but the device permissions are easy to eyeball:

```sh
ls -l /dev/uinput
```

```
crw-rw---- 1 root input 10, 223 Jan  1 12:00 /dev/uinput
```

`root input` and `crw-rw----` is what you are looking for. On the input side, `ls -l /dev/input/event*` should show the
same group. If both look right and `id -nG` includes `input`, `voice-orders run` will get as far as creating its
virtual keyboard.

::: tip
voice-orders deliberately creates the uinput device **first**, before it opens your microphone or loads the model, so a
permissions problem fails immediately with an actionable error rather than thirty seconds into loading a 128 MB model.
:::

## Why not just run it with sudo?

Because it types into your desktop session and reads your input devices — it is exactly the sort of program that should
not be running as root. The group-plus-udev-rule setup above gives it access to the two device nodes it needs and
nothing else. `setup` itself elevates only for the individual steps which genuinely need it.

## What you can do without any of this

[`voice-orders validate`](../profiles/README.md#validating-a-profile) needs nothing but a model, and
[`voice-orders test`](../profiles/README.md#rehearsing-a-profile) needs a model and a microphone — it never opens
`/dev/uinput`, because it emits no input events at all. You can write and rehearse a whole profile before touching any
of the system configuration on this page; you only need it when you want the keys to actually land in a game.

## A note on privacy

This deserves stating plainly rather than being buried.

**evdev hotkeys are global.** The listen hotkey is read from the kernel's input layer, which sits below the display
server, so it fires no matter what has focus — including while you are typing into a password field, a chat window, or
another game. That is what makes it work inside fullscreen games; it also means there is no such thing as a
"only while the game is focused" hotkey here.

**A process which reads `/dev/input` can technically observe every keystroke on the machine.** That is a property of
the interface, not of this program, and it applies to any tool which offers global hotkeys on Linux. What voice-orders
does with that access is narrow and deliberate: the hotkey task discards every event whose type is not `EV_KEY`, and
every key event whose code is not the single key your profile configured, **without its value ever being inspected or
logged**. Nothing else about your typing is read, stored, buffered or transmitted.

The same applies on the audio side. Recognition is constrained to a grammar built from your own profile, it happens
entirely on your machine, and no audio ever leaves it.

If you would rather not grant `/dev/input` access at all, you can leave the [`hotkey:`](../profiles/README.md#hotkey)
block out of your profile entirely — voice-orders then listens continuously and never opens an input device. You still
need `/dev/uinput` for the output side.

## Troubleshooting

Start with `voice-orders doctor`, which names the failing check and what to do about it. The common cases:

### "We were not allowed to read /dev/input/eventN"

You are not in the `input` group yet, or your session predates joining it. Run `voice-orders setup` (or the `usermod`
command above) and log out and back in.

### "Permission denied" creating the virtual keyboard

The udev rule is missing, or `/dev/uinput` was created before the rule was added. `voice-orders setup` adds and reloads
it; by hand, see [step 3](#step-3-add-the-udev-rule) and reload the module afterwards.

### "No such file or directory" for /dev/uinput

The `uinput` module is not loaded. See [step 2](#step-2-load-the-uinput-module).

### The hotkey does nothing

The device the hotkey lives on may not be the one being watched. `voice-orders doctor drg.yaml` will tell you whether
your profile's hotkey device resolves at all. To find the right one, run `sudo evtest`, press your hotkey, and note
which device reports it; then set [`hotkey.device`](../profiles/README.md#hotkey-device) to part of that device's name,
or to its exact `/dev/input/eventN` path. Device numbers change when hardware is unplugged and replugged, so a name
substring is the more durable choice.
