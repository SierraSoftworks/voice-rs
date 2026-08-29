# Validating a Profile

```sh
voice-orders validate drg.yaml
```

`validate` is the fastest feedback loop you have. It loads a profile, compiles its
[grammar](../grammar/README.md), checks every word of it against your model's vocabulary, and tells you how the profile
will behave — **everything it finds in one pass**, so you are never fixing problems one reload at a time.

It exits `1` if anything was an error and `0` when there were only warnings and notes, which makes it easy to run over
a repository of profiles in CI. A missing model is deliberately *not* allowed to stop the rest: it is reported as one
more finding, so a broken grammar is never hidden behind a download you have not done yet.

::: tip
`validate` checks what a profile *says*. [`test`](../profiles/README.md#rehearsing-a-profile) checks what it *does*,
with your voice and your microphone but without touching your keyboard, and [`doctor`](./permissions.md) checks the
machine underneath both. The three cover different failures and are worth running in that order.
:::

## The report

The header names the profile and where it was loaded from. Anything which is not attributable to a single command comes
first, then one section per **published rule**, then a summary line:

```
drg.yaml — Deep Rock Galactic
  warning: Nothing refers to the private rule 'dead', so it can never be spoken.

Autocannon
  note: compiles into 30 automaton states
  note: saying "autocannon" will wait 500ms in case you continue with "autocannon sentry"

Terminal
  note: compiles into 15 automaton states

Salute
  note: compiles into 7 automaton states

3 commands checked — 0 errors, 1 warning.
```

A rule with nothing to say about it reports `ok`. Private rules have no section of their own; findings about them are
reported profile-wide, under the header.

## Grammar lints

The [static analysis](../grammar/README.md#what-is-only-a-warning) which runs when the profile loads reports
constructions which are legal — the profile runs — but almost always a mistake. They never fail validation on their
own:

| Warning | What it means |
|---|---|
| `Nothing refers to the private rule 'dead', so it can never be spoken.` | A lowercase rule no other rule uses. Reference it, publish it by giving it a TitleCase name, or delete it. |
| `This block splices everything with '...' and also splices captures by name — the captured presses will play twice.` | Naming a [capture](../grammar/README.md#captures) does not remove it from `...`. Keep the bare splice or the named ones, not both. |
| `You hold 'leftctrl' here but never release it — the keys stay down after the command finishes.` | A hold-style macro, which is a real thing to want — or a missing `release(..)`. voice-orders releases everything it holds when it shuts down either way. |

## Compile diagnostics

Some problems only appear once the grammar is compiled into the automaton the matcher walks. These are **errors**, in
`validate`, `test` and `run` alike, and each is rendered with the excerpt of your grammar it is about:

```
Error: Your commands 'Rearm' and 'Reload' can both match "reload", but they press different keys — we couldn't
       tell which one you meant.
   ╭─[ <unknown>:2:1 ]
   │
 2 │ Rearm = "reload" { t }
   │ ──┬──
   │   ╰──── Your commands 'Rearm' and 'Reload' can both match "reload", but they press different keys …
   │
   │ Help: Reword one of the phrases so that every spoken phrase belongs to exactly one command, or give both
   │       commands the same keys if they are deliberate synonyms.
───╯
```

Two commands accepting the same words with the **same** keys are fine — that is how deliberate synonyms are written —
and so is one word carrying different keys in different *contexts*, which is what keeps Arma's team colours working.
What is reported is a spoken phrase which could run two different things.

The other compile error is size: a grammar past
[200,000 automaton states](../grammar/README.md#limits) fails to load, naming the largest rules. A runaway repetition
is the usual cause.

## Vocabulary

Every distinct word in your grammar is looked up in the model's vocabulary. A word the model has never heard can never
be recognized, so it is an error — with suggestions, offered in this order:

1. **Normalization.** The word is lowercased and stripped of punctuation (`.` `,` `!` `?` `'` `"`); if *that* is a word
   the model knows, it is suggested. This catches punctuation pasted into a literal.
2. **Compound decomposition.** For every way of splitting the word in two, if the model knows both halves, the split is
   suggested — `autocannon` → `'auto cannon'`. At most two splits are offered, most balanced first.
3. **Nearest known words.** If the model ships a readable word list at `<model>/graph/words.txt`, candidates within two
   edits are ranked (preferring a shared first letter) and the closest three are offered.

```
error(usr): The model at '…/vosk-model-en-us-0.22-lgraph' does not know the word 'sitrep', so no command
            using it can ever be recognized. Did you mean 'sit rep', 'strep', 'satrap', 'sibrel'?
```

The usual fix is to offer the model both spellings and let it pick:

```
Sitrep = subject ("sit rep" | "report in" | "report status") { ..., 5, 5 }
```

The word set is walked from the rule graph directly, so this costs the same whether a rule has three concrete phrases
or three trillion.

The third kind of suggestion depends on your model. `vosk-model-en-us-0.22-lgraph` ships a readable word list;
`vosk-model-small-en-us-0.15` does not. When it is unavailable and something was unknown, the report says so once, at
the profile level, rather than apologising under every word:

```
note: this model does not ship a readable word list (<model>/graph/words.txt), so we cannot
      suggest the words it does know — only spelling fixes and compound splits
```

FST machinery in the word list (`<eps>`, `<unk>`, `#0` and friends) is filtered out before ranking — suggesting those
would be worse than suggesting nothing. If a word is genuinely outside the model's vocabulary, a larger model is the
other answer; see [choosing a model](./installation.md#which-model-to-use).

## Notes: how the profile will behave

Notes are never failures. They exist because the interesting properties of a composable grammar are not visible by
reading it.

### Automaton size

```
Select
  note: compiles into 1038 automaton states
```

One note per published rule, so the cost of a rule which inlines a shared subject — or a repetition sitting at its
bound — is something you can see rather than guess at. Arma's forty subject-led commands land around a thousand states
each; a simple command is a few dozen.

### Decompositions

```
note: the rule 'subject' expands into 5389473684224 concrete phrases (more than the 512 the recognizer
      is fed whole), so it is decomposed into fragment phrases for recognition
```

The recognizer is fed whole phrases wherever a rule is small enough for that, because whole utterances recognize best.
A rule over the cap is instead [decomposed into fragments](../grammar/README.md#how-the-grammar-reaches-the-recognizer)
at its referenced-rule boundaries, and the automaton — not Vosk — decides which fragment sequences form a real command.
That is a genuine trade of recognition accuracy for feasibility, which is why it is reported rather than done quietly.
It is not something to fix; it is something to know about the profile you wrote.

### Prefix relations

```
Select
  note: saying "all" will wait 500ms in case you continue with "all hide"
```

Where the [completion timeout](../profiles/README.md#completion-timeout) is actually paid: a point at which one command
is already complete while a longer one is still possible. The note quotes *your* configured timeout, and names a
witness phrase and where continuing leads.

::: warning These notes are a bounded sweep, not a proof
Finding every prefix relation in a composable grammar means exploring the automaton, and a large grammar has more of it
than is worth exploring. Both this sweep and duplicate detection work to a fixed budget, breadth-first — so the short
phrases where a wait is actually felt are covered first, and at most one witness per rule is reported.

The consequence is worth stating plainly: **the absence of a note is not proof of absence.** A very large grammar may
have ambiguous points the sweep never reached, and duplicate detection witnesses each spot with a single word sequence,
so two rules which agree on the witness but differ elsewhere can go unreported.
:::

## In CI

`validate` takes one profile at a time, so a repository of them is a loop:

```sh
for profile in profiles/*.yaml; do
  voice-orders validate "$profile" || exit 1
done
```

Because the exit code is `1` only for errors, warnings and notes will not fail a build. The model is the one thing CI
needs beyond the binary: without it the vocabulary check reports "we could not find a Vosk speech model" as an error
and everything else still runs, so a job which only wants the grammar checked can pass `--model` at a cached model
directory, or accept that one finding.

`--model <path-or-name>` overrides the profile's `model:` field and `$VOSK_MODEL_PATH`, exactly as it does on `test`
and `run`.
