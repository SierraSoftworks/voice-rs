# Grammar

Every command's `phrase:` is written in a small domain-specific language. It exists so that one command can cover the
several ways you might actually say something, without you writing out each variation by hand — and so that voice-orders
can compile the whole profile into a recognition grammar which contains **only** the phrases it can act on.

That last part is what makes recognition as accurate as it is. The recognizer is never asked to transcribe free speech
and hope; it is asked to decide between the handful of things your profile understands.

## The syntax

```
deploy [the] {autocannon, auto cannon} [sentry]
```

- **Plain words** are required, in the order they are written.
- **`[optional]` groups** may be left unsaid entirely.
- **`{alternate, choices}` groups** require exactly one of their branches.
- The two **nest freely**: `[{optional, elective}] combinations` is a perfectly good phrase.

Whitespace separates words and is otherwise insignificant. Words may contain letters, digits, apostrophes and hyphens.

The phrase above matches all of:

```
deploy autocannon
deploy the autocannon
deploy auto cannon
deploy the auto cannon
deploy autocannon sentry
deploy the autocannon sentry
deploy auto cannon sentry
deploy the auto cannon sentry
```

## EBNF

```ebnf
phrase     = term , { term } ;
term       = word | optional | alternates ;
optional   = "[" , phrase , "]" ;
alternates = "{" , phrase , { "," , phrase } , "}" ;
word       = word-char , { word-char } ;
word-char  = letter | digit | "'" | "-" ;
```

Nesting is capped at **8 levels**. Deeper phrases are almost certainly a mistake, and the cap keeps the parser's
recursion bounded.

## Worked examples

### An optional article

```yaml
phrase: open [the] terminal
```

Matches "open the terminal" and "open terminal". Useful everywhere, because how strictly you articulate an article is
not something you want to think about mid-firefight.

### Alternate spellings of a compound

```yaml
phrase: deploy {autocannon, auto cannon}
```

Speech models frequently do not know compound words, but do know both halves. Offering both spellings is the standard
fix — and it is the fix `validate` will suggest to you when it finds one.

### An optional suffix which is also its own command

```yaml
commands:
  - phrase: reload
    keys: ["r"]

  - phrase: reload weapon
    keys: ["leftshift+r"]
```

Two separate commands where one phrase is a prefix of the other. This is the case the
[completion timeout](#ambiguity-and-the-completion-timeout) exists for.

### Alternates with more than one word per branch

```yaml
phrase: "{call in, request} [a] resupply"
```

Each branch of an alternates group is a whole phrase, not a single word, so branches may differ in length.

::: tip
Quote a phrase which **starts** with `{` or `[`. YAML would otherwise read the leading brace or bracket as the start of
a flow collection and fail to parse the line. A phrase whose groups appear anywhere after the first character —
`deploy [the] {autocannon, auto cannon}` — needs no quoting.
:::

### Nesting

```yaml
phrase: deploy [the {left, right}] turret
```

Matches "deploy turret", "deploy the left turret" and "deploy the right turret" — but not "deploy the turret", because
the alternates group is required *inside* the optional group.

### A phrase with no required words

```yaml
phrase: "[deploy] [the] [sentry]"
```

This is an **error**. Every term being optional means the phrase expands to include the empty phrase, which nobody can
say. Move at least one word outside its brackets:

```yaml
phrase: deploy [the] [sentry]
```

## Expansion

Before recognition starts, each phrase is expanded into the concrete word sequences the recognizer will listen for. An
`[optional]` group contributes both its omission and its contents; an `{alternates}` group contributes each of its
branches; the results are the cartesian product, deduplicated in insertion order. So `[a] [a]` and `{a, a}` collapse
their duplicates rather than listing the same phrase twice.

Expansion grows multiplicatively, which is easy to do by accident:

| Limit | Severity | What happens |
|---|---|---|
| More than **128** concrete phrases | warning | The command still works, but the grammar is getting large, and a command with that many ways to say it is easier to trigger by accident. |
| More than **512** concrete phrases | error | Rejected outright, in `validate` and in `run`. Split the command up or remove some groups. |

The count is computed *multiplicatively, before anything is materialized*, so an explosive phrase such as ten chained
four-way alternates fails immediately with its real count (1,048,576) rather than trying to allocate it. Truncating the
grammar instead would silently break commands, which is worse than refusing.

## Out-of-grammar speech

The compiled grammar contains every expanded phrase of every command, plus a special `[unk]` entry. That entry matters
more than it looks: without it, the recognizer force-aligns *any* speech onto the nearest grammar phrase, so unrelated
chatter on voice comms turns into false triggers. With it, out-of-grammar audio decodes as `[unk]` and is discarded.

## Ambiguity and the completion timeout

A phrase is **ambiguous** when it is a strict word-prefix of some other phrase in the profile — that is, when everything
you have said so far is a complete command *and* the beginning of a longer one. With both `reload` and `reload weapon`
in a profile, saying "reload" is ambiguous; saying "salute" is not.

Unambiguous commands fire the moment the recognizer finalizes the utterance. An ambiguous one waits for
[`completion_timeout`](../profiles/README.md#completion-timeout) (300ms by default) to see whether you are still
talking:

- **You carry on with "weapon"** → the longer command supersedes the short one, and only `reload weapon` fires.
- **You stop** → the timer expires and `reload` fires.
- **You say something else entirely** → `reload` fires first, then the new words are matched from the start.
- **You mute mid-way** → the pending command is discarded. A half-confirmed command must never fire when you unmute.

With [eager matching](../profiles/README.md#recognition-eager) on (the default), the timer starts the moment the
recognizer's in-progress hypothesis comes to rest on the ambiguous phrase — you pay the timeout from when you pause,
not from when the recognizer finalizes. And a hypothesis which continues to extend the pending phrase supersedes the
short command directly, without waiting for finalization at all. With eager matching off, commands only ever fire on
finalized results, and partial results are used solely to *hold the timer open* — if what you are still saying
continues to extend the pending phrase, the deadline is pushed out so the short command does not fire underneath you.

`voice-orders validate` prints a note for every prefix relation in your profile, quoting your configured timeout, so
this behaviour is discoverable per profile rather than something you have to infer:

```
reload
  note: saying "reload" will wait 350ms in case you continue with "reload weapon"
```

### A note on latency

Where the time actually goes: the recognizer's in-progress hypothesis usually settles on your exact final words
**hundreds of milliseconds before** its silence endpointer finalizes the utterance. Three
[`recognition:`](../profiles/README.md#recognition) options decide how much of that time you get back.

**Eager matching claims most of it.** With [`eager`](../profiles/README.md#recognition-eager) on (the default),
"reload weapon" spoken in one breath fires [`eager_delay`](../profiles/README.md#recognition-eager-delay) (100ms by
default) after the hypothesis stops changing — no finalization involved. The trade is honesty about the rare miss: a
hypothesis the recognizer later revises has already pressed its keys, and the session reports a `warning:` naming
what fired versus what was actually said.

**The endpointer sets the floor for everything else.** With eager off, every command waits out
[`recognition.silence`](../profiles/README.md#recognition-silence) (200ms by default; Vosk's own default is roughly
half a second) after your last word before it can fire.

**The completion timeout only costs you when you pause.** It engages only when you actually stop between "reload" and
"weapon" — and with eager matching on, it now runs from the moment you pause rather than from the later finalization,
so the short command's perceived latency is just your `completion_timeout`. If even that wait bothers you, the
cheapest fix is still to make the ambiguity go away — rename one of the two commands so that neither phrase is a
prefix of the other.

**When you would rather trade latency for certainty**, [`alternatives`](../profiles/README.md#recognition-alternatives)
turns the equation around: the recognizer's n-best readings of each finalized utterance are compared, and an utterance
whose close runner-up would have run a *different* command is suppressed with a warning instead of guessed at. Because
it needs the finalized result, it cannot be combined with eager firing.

## How `validate` checks your words

Every distinct word in your phrases is looked up in the model's vocabulary. A word the model has never heard can never
be recognized, so it is reported as an error under the command that used it — with suggestions, offered in this order:

1. **Normalization.** The word is lowercased and stripped of punctuation (`.` `,` `!` `?` `'` `"`); if *that* is a word
   the model knows, it is suggested. This catches punctuation pasted into a phrase.
2. **Compound decomposition.** For every way of splitting the word in two, if the model knows both halves, the split is
   suggested — `autocannon` → `'auto cannon'`. At most two splits are offered, most balanced first.
3. **Nearest known words.** If the model ships a readable word list at `<model>/graph/words.txt`, candidates within two
   edits are ranked (preferring a shared first letter) and the closest three are offered.

The third of those depends on your model. `vosk-model-en-us-0.22-lgraph` ships a readable word list;
`vosk-model-small-en-us-0.15` does not. When it is unavailable and something was unknown, the report says so once, at
the profile level, rather than apologising under every word:

```
note: this model does not ship a readable word list (<model>/graph/words.txt), so we cannot
      suggest the words it does know — only spelling fixes and compound splits
```

FST machinery in the word list (`<eps>`, `<unk>`, `#0` and friends) is filtered out before ranking — suggesting those
would be worse than suggesting nothing.

The usual fix for an unknown word is the alternates group: offer the model both spellings and let it pick.

```yaml
phrase: deploy {autocannon, auto cannon}
```

If a word is genuinely outside the model's vocabulary, a larger model is the other answer — see
[choosing a model](../guide/installation.md#which-model-to-use).

## Error messages

Phrases are parsed while the profile loads, so a syntax error is a load-time failure with a precise location rather than
a surprise at runtime. The parser is deliberately specific about what went wrong:

| Situation | Message |
|---|---|
| Unclosed `[` | `You have an unclosed '[' at line 1, column 8 — every optional group needs a matching ']'.` |
| Stray `]` | `We found a ']' at line 1, column 3 without a matching '[' before it.` |
| Empty alternate branch | `The alternates group starting at line 1, column 8 has an empty branch (a ',' with nothing before or after it).` |
| Empty optional group | `The optional group at line 1, column 8 is empty.` |
| Too much nesting | `You've nested groups more than 8 levels deep at line 1, column 41.` |
| An invalid character | `We found an unexpected character '(' at line 1, column 12.` |

Each comes with advice and, where it helps, a worked example of the correct form.
