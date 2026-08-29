# Key Reference

Every key name voice-orders accepts is listed on this page. There are **121** of them, and they are the same names
everywhere: in a grammar rule's [action block](../grammar/README.md#action-blocks), inside `hold(..)` and `release(..)`,
and in [`hotkey.key`](../profiles/README.md#hotkey-key).

The naming rule is simple: **the lowercase evdev key name with its `KEY_` prefix removed**. `KEY_LEFTCTRL` becomes
`leftctrl`, `KEY_F5` becomes `f5`, `KEY_KP1` becomes `kp1`. If you have ever read `evtest` output, you already know
these names.

Names are matched exactly and are case-sensitive: `LEFTCTRL` and `leftcontrol` are both errors.

## Chords

An action may be a single key or a **chord** — several key names joined with `+`:

```yaml{3,5}
grammar: |
  Terminal = "open" "the"? "terminal" { leftctrl+leftalt+t }

  CycleWeapons = "cycle weapons" { 1, 2, 3 }
```

A chord goes **down in the order written**, is held for
[`defaults.duration`](../profiles/README.md#defaults-duration), then comes **up in reverse order**, so modifiers
outlive the key they modify. Consecutive presses are separated by
[`defaults.interval`](../profiles/README.md#defaults-interval).

`hold(..)` and `release(..)` take a chord too, and are how you express a press whose two edges belong to different
moments:

```yaml{2}
grammar: |
  Salute = "salute" { hold(x), wait(750ms), release(x) }
```

::: warning
[`hotkey.key`](../profiles/README.md#hotkey-key) takes a **single key name**, not a chord. A `+` there is a load error.
:::

## Did you mean …?

An unrecognized key name fails when the profile loads, with a suggestion where there is a plausible one:

```
We don't recognize 'leftctlr' as a key name. Did you mean 'leftctrl'?
```

Candidates are ranked by edit distance (at most two edits), preferring names which share your first letter, then those
closest in length. Something which is not a typo for anything — `mouse_wheel_up`, say — gets no suggestion rather than
a misleading one. Mouse and gamepad output are not supported; voice-orders emits keyboard events only.

## Letters

<Badge text="26 keys"/>

`a` `b` `c` `d` `e` `f` `g` `h` `i` `j` `k` `l` `m` `n` `o` `p` `q` `r` `s` `t` `u` `v` `w` `x` `y` `z`

These are physical keys, not characters: `a` is the key labelled A, and pressing it with `leftshift` held is how you get
a capital.

## Digits

<Badge text="10 keys"/>

`0` `1` `2` `3` `4` `5` `6` `7` `8` `9`

The number row above the letters. The keypad digits are [separate keys](#keypad) — games very often bind them
differently.

::: tip
Digit keys need no quoting inside a grammar action block — `{ 4 }` is the key labelled 4, because the block is grammar
text rather than YAML.
:::

## Function keys

<Badge text="24 keys"/>

`f1` `f2` `f3` `f4` `f5` `f6` `f7` `f8` `f9` `f10` `f11` `f12` `f13` `f14` `f15` `f16` `f17` `f18` `f19` `f20` `f21`
`f22` `f23` `f24`

`f13` through `f24` do not exist on most physical keyboards, which makes them useful bindings for a virtual one — you
can bind them in a game knowing nothing else will ever press them.

## Typing keys and punctuation

<Badge text="17 keys"/>

| Name | Key |
|---|---|
| `space` | Space bar |
| `enter` | Return / Enter (the main one — the keypad has [its own](#keypad)) |
| `esc` | Escape |
| `tab` | Tab |
| `backspace` | Backspace |
| `minus` | `-` / `_` |
| `equal` | `=` / `+` |
| `leftbrace` | `[` / `{` |
| `rightbrace` | `]` / `}` |
| `semicolon` | `;` / `:` |
| `apostrophe` | `'` / `"` |
| `grave` | `` ` `` / `~` (the backtick key, above Tab) |
| `backslash` | `\` / `\|` |
| `comma` | `,` / `<` |
| `dot` | `.` / `>` |
| `slash` | `/` / `?` |
| `capslock` | Caps Lock |

These are the US-layout labels. voice-orders emits key codes, not characters, so what a key actually types depends on
the keyboard layout the receiving application is using.

## Modifiers

<Badge text="8 keys"/>

| Name | Key |
|---|---|
| `leftctrl` | Left Control |
| `rightctrl` | Right Control |
| `leftshift` | Left Shift |
| `rightshift` | Right Shift |
| `leftalt` | Left Alt |
| `rightalt` | Right Alt / AltGr |
| `leftmeta` | Left Super / Windows / Command |
| `rightmeta` | Right Super / Windows / Command |

There is no generic `ctrl`, `shift`, `alt` or `meta` — sides are distinct at the evdev level, and some games
distinguish them. `leftctrl` is the conventional choice when you do not care.

Modifiers in a chord are written first, because a chord presses its keys in the order you wrote them:
`"leftctrl+leftshift+p"`.

## Arrows

<Badge text="4 keys"/>

`up` `down` `left` `right`

## Navigation cluster

<Badge text="6 keys"/>

| Name | Key |
|---|---|
| `insert` | Insert |
| `delete` | Delete |
| `home` | Home |
| `end` | End |
| `pageup` | Page Up |
| `pagedown` | Page Down |

## System keys

<Badge text="4 keys"/>

| Name | Key |
|---|---|
| `sysrq` | Print Screen / SysRq |
| `scrolllock` | Scroll Lock (three `l`s: `scroll` + `lock`) |
| `pause` | Pause / Break |
| `numlock` | Num Lock |

## Keypad

<Badge text="16 keys"/>

| Name | Key |
|---|---|
| `kp0` … `kp9` | Keypad digits `0`–`9` |
| `kpslash` | Keypad `/` |
| `kpasterisk` | Keypad `*` |
| `kpminus` | Keypad `-` |
| `kpplus` | Keypad `+` |
| `kpenter` | Keypad Enter |
| `kpdot` | Keypad `.` / Del |

In full: `kp0` `kp1` `kp2` `kp3` `kp4` `kp5` `kp6` `kp7` `kp8` `kp9` `kpslash` `kpasterisk` `kpminus` `kpplus`
`kpenter` `kpdot`.

Keypad keys are distinct from the [digit row](#digits) — a game bound to `1` will not respond to `kp1`.

::: tip
What the keypad digits report depends on Num Lock on a physical keyboard. The virtual keyboard voice-orders creates has
no Num Lock state of its own, so it simply emits the keypad codes and lets the receiving application decide.
:::

## Media keys

<Badge text="6 keys"/>

| Name | Key |
|---|---|
| `mute` | Mute |
| `volumeup` | Volume Up |
| `volumedown` | Volume Down |
| `playpause` | Play / Pause |
| `nextsong` | Next Track |
| `previoussong` | Previous Track |

Handy for the sort of command that has nothing to do with the game — "pause the music" is a perfectly good voice macro.
