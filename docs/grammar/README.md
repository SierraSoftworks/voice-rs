# Grammar

A profile's [`grammar:`](../profiles/README.md#grammar) block is a small language for describing the commands you can
speak. It exists so that one command can cover the several ways you might actually say something, so that forty
commands can share the phrase they all start with, and so that voice-orders can compile the whole profile into a
recognition grammar containing **only** the things it can act on.

That last part is what makes recognition as accurate as it is. The recognizer is never asked to transcribe free speech
and hope; it is asked to decide between the handful of things your profile understands.

```yaml
grammar: |
  // "deploy the autocannon", "auto cannon sentry", ...
  Autocannon = "deploy"? "the"? ("autocannon" | "auto cannon") "sentry"? { 4 }

  Terminal = "open" "the"? "terminal" { leftctrl+leftalt+t }

  Salute = "salute" { hold(x), wait(750ms), release(x) }
```

## Rules

A grammar is a list of **rules**. Each one is a name, an `=`, a pattern saying what to listen for, and an optional
`{ ... }` action block saying what to press:

```
name = pattern { actions }
```

Rule names are made of letters, digits and underscores, and the first letter decides everything about how the rule is
used:

| Name | Kind | Meaning |
|---|---|---|
| `Autocannon`, `ReturnToFormation` | **published** | A speakable command. Saying it fires its action block. |
| `subject`, `squad_number` | **private** | A building block other rules refer to. Never a command on its own. |

Nothing else marks a command: a leading capital publishes the rule, anything else keeps it private. That is what lets a
grammar grow a shared vocabulary without every fragment of it becoming something you can accidentally trigger.

`//` starts a comment which runs to the end of the line, and rules have no terminator — a rule ends where the next
`name =` begins:

```
// A subject spoken on its own just selects those units.
Select = subject

ReturnToFormation = subject ("return to formation" | "form up" | "regroup") { ..., 1, 1 }
```

::: tip
`grammar:` is a YAML block scalar (`grammar: |`), so everything under it is text and nothing in it needs quoting or
escaping for YAML's sake. Keep the indentation consistent and write the grammar as you would write code.
:::

## Patterns

### Literals

A double-quoted literal is spoken words. Multi-word literals are fine and are the normal way to write a phrase:

```
Map = "map" | "toggle map" | "show map" | "hide map" { m }
```

Literals are lowercased and split on whitespace when they load, so `"Fall Back"` and `"fall back"` are the same two
words. Words may contain letters, digits, apostrophes and hyphens — `"i'm hit"`, `"auto-cannon"` — and each literal
must close on the line it opened.

### Sequences

Terms written one after another must be spoken in that order:

```
Terminal = "open" "the"? "terminal" { leftctrl+leftalt+t }
```

### Alternation

`|` separates alternatives; exactly one branch is spoken:

```
Lights = "lights" | "light" | "flashlight" | "torch" | "laser" { l }
```

At rule level, a trailing action block belongs to the **whole rule**, not to the last branch — every branch of `Lights`
presses `l`.

### Groups

Parentheses group alternatives inside a longer pattern:

```
Advance = subject ("advance" | "move up") { ..., 1, 2 }
AntiMaterielRifle = "anti" ("materiel" | "material") "rifle" { down, left, right, up, down }
```

A group's branches may each carry their **own** action block, which is how one rule holds a small table of
word-to-key mappings:

```
squad_number = ( "one"   { f1 }
               | "two"   { f2 }
               | "three" { f3 } )
```

An action block inside a group binds to the branch it terminates. At rule level it binds to the rule.

### Rule references

A bare name refers to another rule, and brings in **both** its words and whatever it accumulated:

```
subject_all = "all" | "everyone" | "team" | "squad" { grave }
subject = subject_all | squad_selection | team_selection

Advance = subject ("advance" | "move up") { ..., 1, 2 }
```

Rules may not refer back to themselves, directly or through a chain of other rules. Recursion is a load error which
points you at bounded repetition instead.

### Repetition

Every repetition is **bounded**, which is what keeps a grammar finite by construction:

| Written | Means |
|---|---|
| `term?` | `[0..1]` — may be left unsaid |
| `term*` | `[0..8]` |
| `term+` | `[1..8]` |
| `term[3]` | exactly three times |
| `term[1..4]` | between one and four times |
| `term[2..]` | at least twice, up to the global cap of 8 |
| `term[..4]` | up to four times, possibly none |

`?`, `*` and `+` are sugar, and the missing end of `[n..]` is filled in, using a global cap of **8**. Bounds you write
out in full may exceed it — the cap exists to make the shorthand finite, not to limit what you may say outright:

```
// Articulate bounds squad selection at ten members; the repetition matches it
// exactly, and each iteration's presses append in spoken order.
squad_selection = squad_number ("and"? squad_number)[0..9]
```

A repetition which can never match anything (`[0]`, or `[3..1]`) is a load error.

### Captures

`term:name` **captures** a term. Everything that term contributes is collected under that name so an action block can
place it exactly where it belongs:

```
Watch = subject:sub ("watch" | "watch the") direction:dir { sub..., 3, 8, wait(20ms), dir... }
```

A capture may name any term, including a whole group:

```
Assign = subject:sub ("assign" | "assign to" | "add to" | "switch to") ("team"? assign_colour):colour { sub..., 9, colour... }
```

Captured words also show up in the name a match is reported by — `Watch(two and three, north)` — which makes a log or a
rehearsal say what you actually said, not just which rule matched.

Two rules about captures:

- **Each name is used once per rule.** Capturing `:x` twice in one rule is a load error; splicing a name nothing
  captured is too, with a "did you mean?" for near misses.
- **A capture in an unmatched optional is empty**, and a capture inside a repetition collects every iteration. Neither
  is an error — an empty splice simply contributes nothing.

## Action blocks

An action block is a comma-separated list, written in `{ }`, saying what a match does:

```
Salute = "salute" { hold(x), wait(750ms), release(x) }
```

| Action | Effect |
|---|---|
| `4`, `m`, `leftctrl+leftalt+t` | Press a key or a **chord**: every key down in the order written, held for [`defaults.duration`](../profiles/README.md#defaults-duration), then up in reverse order so modifiers outlive the key they modify. |
| `wait(20ms)` | Pause. Durations are written the way you would say them: `20ms`, `1s`, `1s 500ms`. |
| `hold(x)` | Press without releasing. |
| `release(x)` | Release without pressing. |
| `release(*)` | Release **every** key the virtual keyboard is currently holding. |
| `...` | Splice in everything the matched words accumulated. |
| `name...` | Splice in one [capture](#captures). |

`wait`, `hold` and `release` are reserved in action position; every other bare name is a
[key name](../keys/README.md), and one we do not recognize is a load error with a suggestion
(`We don't recognize 'leftctlr' as a key name. Did you mean 'leftctrl'?`).

### What a match accumulates

Walking a match from left to right, each matched term appends what it contributes to the command's **accumulated
vector**: a group branch appends its inline block, a rule reference appends that rule's result, a plain literal appends
nothing. A repetition appends once per iteration, in spoken order.

The rule's own action block, evaluated after the whole command has matched, then decides what actually runs. `...`
splices the accumulation in as it stands; `name...` splices one capture. Both may be used as many times as you like.

A rule with **no** action block implicitly propagates — it behaves exactly as if you had written `{ ... }`. That is how
`subject` hands each subject's key presses to the forty commands which start with one:

```
squad_number = ( "one" { f1 } | "two" { f2 } | "three" { f3 } )
squad_selection = squad_number ("and"? squad_number)[0..9]
subject = subject_all | squad_selection | team_selection

// "two and three advance" → f2, f3, then the move menu and its entry
Advance = subject ("advance" | "move up") { ..., 1, 2 }
```

::: warning The double-splice trap
Naming a capture does **not** remove it from `...`. A block using both a bare `...` and a `name...` splices those
presses **twice**:

```
// Wrong: sub's presses play once for the '...' and again for 'sub...'
Advance = subject:sub ("advance" | "move up") { ..., 1, 2, sub... }
```

It is legal — and so it loads — but it is almost never what anyone means, so it is reported as a warning:
*"This block splices everything with `...` and also splices captures by name — the captured presses will play twice."*
Keep either the bare splice or the named ones, not both.
:::

### Ordering with captures

The reason to name a capture is placement. `Watch` has to select the subject, open the engage menu, give the menu a
beat to appear, and only then press the direction — which a bare `...` cannot express, because it would splice the
subject *and* the direction together before the menu ever opened:

```
direction = ( "north"      { 1 }
            | "north east" { 2 }
            | "east"       { 3 }
            | "south east" { 4 } )

Watch = subject:sub ("watch" | "watch the") direction:dir { sub..., 3, 8, wait(20ms), dir... }
```

Said as "two watch north east", that runs `f2`, `3`, `8`, a 20ms pause, then `2`.

### The same word, different keys

Because the automaton is not determinized, a word can carry different output depending on where it appears. Arma's
colours are the canonical case: as a *subject* a colour selects a team, as the direct object of an assignment it
presses the plain menu number.

```
team_colour  = ( "red" { leftshift+f1 } | "green" { leftshift+f2 } | "blue" { leftshift+f3 } )
assign_colour = ( "red" { 1 }           | "green" { 2 }            | "blue" { 3 } )
```

Context decides which reading survives. What is *not* allowed is genuine ambiguity: two published rules accepting the
same words with **different** presses is an error when the automaton compiles, naming both rules and a witness phrase.
Two rules accepting the same words with the *same* presses collapse quietly, which is how deliberate synonyms are
written.

## Pacing

The assembled plan is flattened and then paced by the profile's [`defaults`](../profiles/README.md#defaults):

- [`defaults.duration`](../profiles/README.md#defaults-duration) is how long each press is held;
- [`defaults.interval`](../profiles/README.md#defaults-interval) is the gap left between consecutive presses, including
  across splice boundaries, with no trailing wait after the last one;
- an explicit `wait(..)` **replaces** the implicit interval at that point rather than adding to it;
- `hold`, `release` and `release(*)` carry no implicit pacing at all — they happen when they are written, and because
  the interval belongs *between presses*, one of them standing between two presses means those two are no longer
  consecutive: their spacing is then yours to state with `wait`.

So `Terminal = "open" "the"? "terminal" { leftctrl+leftalt+t }` with the default 30ms duration becomes:

```
down leftctrl, down leftalt, down t, wait 30ms, up t, up leftalt, up leftctrl
```

A `hold` with nothing to release it is legal — that is exactly how a hold-style macro is written — but it is linted, so
you find out about it before your game does. Whatever your macros hold, voice-orders releases every key it is still
holding when it shuts down.

## A worked example: building up a subject

The Arma profile ([`profiles/arma3.yaml`](https://github.com/SierraSoftworks/voice-rs/blob/main/profiles/arma3.yaml))
is forty commands which all begin the same way. It is worth reading in the order it was built.

**1. A table of words and the keys they press.** A private rule with per-branch actions:

```
squad_number = ( "one"   { f1 }
               | "two"   { f2 }
               | "three" { f3 }
               | "four"  { f4 } )
```

**2. Repetition, so several can be selected at once.** "one two and three" presses `f1`, `f2`, `f3` — the optional
"and" contributes no keys, and each iteration appends in spoken order:

```
squad_selection = squad_number ("and"? squad_number)[0..9]
```

**3. The other ways of naming a subject**, and one rule which is any of them. None of these has an action block, so
each propagates what it accumulated:

```
team_colour = ( "red" { leftshift+f1 } | "green" { leftshift+f2 } | "blue" { leftshift+f3 } )
team_selection = ("team" team_colour) | (team_colour "team")
subject_all = "all" | "everyone" | "team" | "squad" { grave }

subject = subject_all | squad_selection | team_selection
```

**4. Publish it once on its own**, because saying a subject and nothing else should just select those units:

```
Select = subject
```

**5. Then every command that acts on a subject** is one line, splicing the selection ahead of its own menu presses:

```
Advance = subject ("advance" | "move up") { ..., 1, 2 }
Stop = subject ("stop" | "hold position" | "halt") { ..., 1, 6 }
OpenFire = subject ("open fire" | "go loud" | "fire at will") { ..., 3, 1 }
```

`Select` is what makes every one of those an ambiguous prefix: "two" is a complete command *and* the start of "two
advance". That is the completion timeout's job, below — and it is exactly the trade the profile chose by publishing
`Select` at all.

## Ambiguity and the completion timeout

A point in the grammar is **ambiguous** when what you have said so far is already a complete command *and* the
beginning of a longer one. With both `Reload` and `ReloadWeapon` in a profile, saying "reload" is ambiguous; saying
"salute" is not. It happens between commands, inside one command (`"north"` is a prefix of `"north east"`), and — most
of all — wherever a bare subject is published alongside the commands built on it.

Unambiguous commands fire as soon as the recognizer's hypothesis settles. An ambiguous one waits for
[`completion_timeout`](../profiles/README.md#recognition-completion-timeout) (750ms by default) to see whether you are still
talking:

- **You carry on with "weapon"** → the longer command supersedes the short one, and only `ReloadWeapon` fires.
- **You stop** → the timer expires and `Reload` fires.
- **You say something else entirely** → `Reload` fires first, then the new words are matched from the start.
- **You mute mid-way** → the pending command is discarded. A half-confirmed command must never fire when you unmute.

With [eager matching](../profiles/README.md#recognition-eager) on (the default), the timer starts the moment the
recognizer's in-progress hypothesis comes to rest on the ambiguous point — you pay the timeout from when you *pause*,
not from when the recognizer finalizes. A hypothesis which continues to extend the pending phrase supersedes the short
command directly, without waiting for finalization at all. With eager matching off, commands only ever fire on
finalized results, and partials are used solely to *hold the timer open*.

`voice-orders validate` names the ambiguous points it finds, quoting your configured timeout, so this is discoverable
per profile rather than something you have to infer:

```
Select
  note: saying "all" will wait 500ms in case you continue with "all hide"
```

::: tip
A profile which publishes a bare subject pays this wait on every subject. If you would rather not, don't publish
`Select` — the subject rule stays perfectly usable as a private building block, and every subject-led command becomes
unambiguous again.
:::

### A note on latency

Where the time actually goes: the recognizer's in-progress hypothesis usually settles on your exact final words
**hundreds of milliseconds before** its silence endpointer finalizes the utterance. Three
[`recognition:`](../profiles/README.md#recognition) options decide how much of that time you get back.

**Eager matching claims most of it.** With [`eager`](../profiles/README.md#recognition-eager) on (the default),
"reload weapon" spoken in one breath fires [`debounce`](../profiles/README.md#recognition-debounce) (100ms by
default) after the hypothesis stops changing — no finalization involved. The trade is honesty about the rare miss: a
hypothesis the recognizer later revises has already pressed its keys, and the session reports a `warning:` naming what
fired versus what was actually said.

**The endpointer sets the floor for everything else.** With eager off, every command waits out
[`recognition.silence`](../profiles/README.md#recognition-silence) (200ms by default; Vosk's own default is roughly
half a second) after your last word before it can fire.

**The completion timeout only costs you when you pause.** It engages only when you actually stop at an ambiguous point
— and with eager matching on it runs from the moment you pause rather than from the later finalization.

**When you would rather trade latency for certainty**, [`alternatives`](../profiles/README.md#recognition-alternatives)
turns the equation around: the recognizer's n-best readings of each finalized utterance are compared, and an utterance
whose close runner-up would have run a *different* command is suppressed with a warning instead of guessed at. Because
it needs the finalized result, it cannot be combined with eager firing.

## How the grammar reaches the recognizer

The recognizer is fed one entry per published rule wherever that is possible: a rule whose concrete phrases number 512
or fewer contributes them whole, which is the best case — Vosk sees complete utterances.

Composition makes that impossible for large rules. `subject` alone admits trillions of forms, so a rule over the cap is
**decomposed at its referenced-rule boundaries** into fragment phrases instead, relying on Vosk chaining grammar
entries within one utterance. The automaton, not Vosk, then decides which fragment sequences form a real command;
invalid orderings decode as clean words and are dropped. It is a real trade of recognition accuracy for feasibility,
which is why [`validate`](../guide/validation.md) reports every rule it had to decompose rather than doing it quietly.

A special `[unk]` entry is always included, and it matters more than it looks: without it the recognizer force-aligns
*any* speech onto the nearest grammar phrase, so unrelated chatter on voice comms turns into false triggers. With it,
out-of-grammar audio decodes as `[unk]` and is discarded.

## Limits

| Limit | Value | What happens |
|---|---|---|
| Repetition cap for `*`, `+` and `[n..]` | 8 | The shorthand's missing bound. Write the bound out in full to exceed it. |
| Concrete phrases fed whole per rule | 512 | Above this the rule is decomposed into fragments for recognition, and `validate` says so. |
| Automaton states (and transitions) | 200,000 | A load error naming the largest rules. The Arma profile lands in the tens of thousands. |
| Simultaneous readings of one utterance | 512 | A runtime guard: the walk is dropped and a warning is reported, rather than stalling. |

## Errors

The grammar is parsed and analyzed while the profile loads, so a mistake is a load-time failure pointing at the exact
place in your grammar — never a command which mysteriously never fires. Every problem is reported in **one pass**: a
syntax error in one rule and an unknown key in another come back together.

```
Error: We don't recognize 'leftctlr' as a key name. Did you mean 'leftctrl'?
   ╭─[ <unknown>:2:36 ]
   │
 2 │ ReloadWeapon = "reload" "weapon" { leftctlr+r }
   │                                    ────┬───
   │                                        ╰───── We don't recognize 'leftctlr' as a key name. Did you mean 'leftctrl'?
   │
   │ Help: Key names are the lowercase evdev key names with their 'KEY_' prefix removed, e.g. 'a', '4', 'f5',
   │       'space', 'enter', 'leftctrl' or 'kp1'. The key reference page in the documentation lists every name
   │       we accept.
───╯
```

### What fails the load

| Situation | Message |
|---|---|
| An undefined rule reference | `You're referring to a rule called 'subjcet', but no rule with that name is defined. Did you mean 'subject'?` |
| The same rule defined twice | `You've defined the rule 'Map' twice — this definition conflicts with an earlier one.` |
| A rule which refers back to itself | `The rule 'a' eventually refers back to itself (a -> b -> a) — grammars can't recurse.` |
| A published rule which can match silence | `'Silent' can match without a single word being spoken — a published command must require at least one word.` |
| A repetition which can never match | `This repetition allows at most zero occurrences, so it can never match anything.` |
| Inverted repetition bounds | `This repetition's minimum (3) is larger than its maximum (1), so it can never match.` |
| An unknown key name in an action | `We don't recognize 'notakey' as a key name.` |
| The same capture name twice in one rule | `You've already captured ':x' earlier in 'R' — each capture in a rule needs its own name.` |
| A splice of a capture which does not exist | `You're splicing 'dri...', but nothing in 'R' is captured as ':dir'. Did you mean 'dir...'?` |
| An unclosed quote, brace or bracket | A syntax error pointing at the place it was opened. |
| Two published rules accepting the same words with different keys | An error when the automaton compiles, naming both rules and a witness phrase. |

### What is only a warning

Lints load fine and are reported by [`validate`](../guide/validation.md), because each of them is legal and
occasionally deliberate:

| Situation | Message |
|---|---|
| A private rule nothing refers to | `Nothing refers to the private rule 'dead', so it can never be spoken.` |
| `...` alongside a `name...` splice | `This block splices everything with '...' and also splices captures by name — the captured presses will play twice.` |
| A `hold` with no `release` in the same block | `You hold 'leftctrl' here but never release it — the keys stay down after the command finishes.` |

Every message keeps the same shape: what we saw, where, and a worked example of the correct form.
