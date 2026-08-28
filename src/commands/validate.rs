//! `voice-orders validate <profile>`: structure, grammar lints, and every word
//! checked against the model's vocabulary. See DESIGN.md §"`validate`".
//!
//! The guiding rule is **one pass, everything reported**: a run tells you about
//! every problem in the profile at once, rather than making you fix them one
//! reload at a time. That is why the model is opened *alongside* the other
//! checks rather than before them — a missing model must not hide a broken
//! phrase — and why the exit code is derived from the finished report instead
//! of from the first thing which went wrong.
//!
//! The vocabulary itself reaches the checks as a `&mut dyn Vocabulary`, so the
//! whole pipeline runs against a `HashSet`-backed fake in tests and only the
//! thin wrapper in [`run`] ever touches libvosk.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use clap::Args;
use tracing_batteries::prelude::*;

use crate::config::{Profile, SystemConfig, duration, loader, resolve_model};
use crate::grammar::expansion;
use crate::output::{CompiledOutput, KeyCode, KeyEvent};
use crate::recognition::{Vocabulary, vosk::VoskVocabulary};

/// Above this many concrete phrases a command still works, but is worth a
/// second look; [`expansion::MAX_EXPANSIONS_PER_COMMAND`] is the hard limit.
const EXPANSION_WARNING_THRESHOLD: usize = 128;

/// The largest edit distance we will still call a plausible mis-hearing.
const MAX_SUGGESTION_DISTANCE: usize = 2;

/// How many nearest-word suggestions to offer for an unknown word.
const MAX_NEAREST_SUGGESTIONS: usize = 3;

/// How many compound decompositions to offer for an unknown word.
const MAX_SPLIT_SUGGESTIONS: usize = 2;

/// Punctuation which people paste into phrases and which the recognizer never
/// sees.
const STRIPPED_PUNCTUATION: &[char] = &['.', ',', '!', '?', '\'', '"'];

#[derive(Args, Debug)]
pub struct ValidateArgs {
    /// The profile to validate: a local path or an https:// URL.
    pub profile: String,

    /// The Vosk model to check the profile's words against.
    /// Overrides the profile's `model:` field and $VOSK_MODEL_PATH.
    #[arg(long)]
    pub model: Option<PathBuf>,
}

/// Validates a profile, printing the report and returning the exit code.
pub async fn run(args: ValidateArgs) -> Result<i32, crate::Error> {
    let loaded = loader::load(&args.profile).await?;

    // Structural problems (a phrase which does not parse, a key name which
    // does not resolve, both output forms at once) stop us here: there is no
    // profile to lint if it did not load.
    let profile = Profile::parse(&loaded)?;

    // The model is the one dependency we cannot fake away. Neither working out
    // *which* model to use nor opening it is allowed to stop the rest: a
    // missing model must not hide a broken phrase, so both failures travel into
    // `check` as the reason we have no vocabulary rather than as early returns.
    // The machine's configuration only matters here for the model: a `model:`
    // written as a bare name is resolved inside its models directory.
    let system = SystemConfig::load()?;

    let report = match resolve_model(args.model.as_deref(), &profile, &system) {
        Ok(model) => match VoskVocabulary::open(&model) {
            Ok(mut vocabulary) => check(&loaded.source, &profile, &model, Ok(&mut vocabulary)),
            Err(e) => check(&loaded.source, &profile, &model, Err(e)),
        },
        // With no model there is no vocabulary, so nothing ever reports a word
        // as unknown and the placeholder path below is never read.
        Err(e) => check(&loaded.source, &profile, Path::new("<no model>"), Err(e)),
    };

    print!("{}", report.render());

    let code = report.exit_code();
    debug!(
        errors = report.errors(),
        warnings = report.warnings(),
        "Validation finished with exit code {code}."
    );

    Ok(code)
}

/// One thing worth saying about a profile or one of its commands.
enum Finding {
    /// Something which is broken: the profile will not run as written.
    Error(crate::Error),
    /// Something which will run, but almost certainly not as intended.
    Warning(String),
    /// Something worth knowing about how the profile will behave.
    Note(String),
}

/// The findings for one command, under the name it will be reported by.
struct Section {
    title: String,
    findings: Vec<Finding>,
}

/// A complete validation report: profile-wide findings, then one section per
/// command, then the summary.
struct Report {
    header: String,
    profile: Vec<Finding>,
    commands: Vec<Section>,
}

impl Report {
    /// The number of errors across the whole report.
    fn errors(&self) -> usize {
        self.count(|finding| matches!(finding, Finding::Error(_)))
    }

    /// The number of warnings across the whole report.
    fn warnings(&self) -> usize {
        self.count(|finding| matches!(finding, Finding::Warning(_)))
    }

    fn count(&self, predicate: impl Fn(&Finding) -> bool + Copy) -> usize {
        self.profile.iter().filter(|f| predicate(f)).count()
            + self
                .commands
                .iter()
                .flat_map(|section| section.findings.iter())
                .filter(|f| predicate(f))
                .count()
    }

    /// Every error message in the report, in the order they are rendered.
    #[cfg(test)]
    fn error_messages(&self) -> Vec<String> {
        self.profile
            .iter()
            .chain(self.commands.iter().flat_map(|s| s.findings.iter()))
            .filter_map(|finding| match finding {
                Finding::Error(e) => Some(e.to_string()),
                _ => None,
            })
            .collect()
    }

    /// `0` when nothing is broken (warnings and notes are fine), `1` otherwise.
    fn exit_code(&self) -> i32 {
        i32::from(self.errors() > 0)
    }

    /// The whole report as it is printed.
    fn render(&self) -> String {
        let mut out = String::new();

        out.push_str(&self.header);
        out.push('\n');
        render_findings(&mut out, &self.profile, None);

        for section in &self.commands {
            out.push('\n');
            out.push_str(&section.title);
            out.push('\n');
            render_findings(&mut out, &section.findings, Some("ok"));
        }

        out.push('\n');
        out.push_str(&self.summary());
        out.push('\n');

        out
    }

    fn summary(&self) -> String {
        let commands = self.commands.len();
        let errors = self.errors();
        let warnings = self.warnings();

        format!(
            "{commands} {} checked — {errors} {}, {warnings} {}.",
            plural(commands, "command", "commands"),
            plural(errors, "error", "errors"),
            plural(warnings, "warning", "warnings"),
        )
    }
}

fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

/// Renders a section's findings, indented, with `empty` shown when there are
/// none (profile-wide findings simply render nothing when there is nothing to
/// say).
fn render_findings(out: &mut String, findings: &[Finding], empty: Option<&str>) {
    if findings.is_empty() {
        if let Some(empty) = empty {
            out.push_str("  ");
            out.push_str(empty);
            out.push('\n');
        }
        return;
    }

    for finding in findings {
        match finding {
            // Errors get the full human-errors treatment — message, cause and
            // advice — indented into the section.
            Finding::Error(e) => {
                for line in human_errors::pretty(e).to_string().lines() {
                    if line.trim().is_empty() {
                        out.push('\n');
                    } else {
                        out.push_str("  ");
                        out.push_str(line);
                        out.push('\n');
                    }
                }
            }
            Finding::Warning(message) => {
                out.push_str("  warning: ");
                out.push_str(message);
                out.push('\n');
            }
            Finding::Note(message) => {
                out.push_str("  note: ");
                out.push_str(message);
                out.push('\n');
            }
        }
    }
}

/// Runs every check against a loaded profile.
///
/// `vocabulary` is either the model's vocabulary or the error which explains
/// why we do not have one; in the latter case every other check still runs and
/// the failure is reported as one more finding.
fn check(
    source: &str,
    profile: &Profile,
    model: &Path,
    vocabulary: Result<&mut dyn Vocabulary, crate::Error>,
) -> Report {
    let header = match profile.name.as_deref() {
        Some(name) => format!("{source} — {name}"),
        None => source.to_string(),
    };

    let mut profile_findings = Vec::new();
    let mut sections: Vec<Section> = profile
        .commands
        .iter()
        .map(|command| Section {
            title: command.display_name().to_string(),
            findings: Vec::new(),
        })
        .collect();

    // Output: every command compiles, and the plan it compiles to makes sense.
    for (index, command) in profile.commands.iter().enumerate() {
        match command.compile(&profile.defaults) {
            Ok(CompiledOutput::Keyboard(plan)) => {
                lint_output(&plan, &mut sections[index].findings);
            }
            Err(e) => sections[index].findings.push(Finding::Error(e)),
        }
    }

    // Grammar: expansions per command, then the relations between them.
    let mut expansions: Vec<Vec<Vec<String>>> = Vec::new();
    for (index, command) in profile.commands.iter().enumerate() {
        match expansion::expand(command.phrase.expr()) {
            Ok(expanded) => {
                lint_expansion(
                    command.display_name(),
                    &expanded.phrases,
                    &mut sections[index].findings,
                );
                expansions.push(expanded.phrases);
            }
            Err(e) => {
                sections[index].findings.push(Finding::Error(e));
                expansions.push(Vec::new());
            }
        }
    }

    lint_phrase_relations(profile, &expansions, &mut sections);

    // Vocabulary: every word the profile listens for must be a word the model
    // can hear.
    match vocabulary {
        Ok(vocabulary) => check_vocabulary(
            profile,
            model,
            vocabulary,
            &mut sections,
            &mut profile_findings,
        ),
        Err(e) => profile_findings.push(Finding::Error(e)),
    }

    Report {
        header,
        profile: profile_findings,
        commands: sections,
    }
}

/// Lints a compiled key plan: unbalanced `down`/`up` steps.
///
/// A `down` with no matching `up` is *legal* — that is exactly how a hold-style
/// macro is written — so it is only a note. An `up` with no preceding `down` is
/// a warning: it will be emitted at a keyboard which is not holding that key,
/// which is almost always a copy-paste slip.
fn lint_output(plan: &[KeyEvent], findings: &mut Vec<Finding>) {
    let mut held: Vec<KeyCode> = Vec::new();

    for event in plan {
        match *event {
            KeyEvent::Down(key) => {
                if !held.contains(&key) {
                    held.push(key);
                }
            }
            KeyEvent::Up(key) => match held.iter().position(|held| *held == key) {
                Some(position) => {
                    held.remove(position);
                }
                None => findings.push(Finding::Warning(format!(
                    "this command releases '{key}' without ever pressing it — check the 'events:' list for a missing 'down: {key}'"
                ))),
            },
            KeyEvent::Wait(_) => {}
        }
    }

    for key in held {
        findings.push(Finding::Note(format!(
            "this command holds '{key}' down and never releases it; that is how a hold-style macro is written, but nothing will let the key go until another command releases it or voice-orders shuts down"
        )));
    }
}

/// Lints one command's expansion: the unspeakable empty phrase, and volume.
fn lint_expansion(name: &str, phrases: &[Vec<String>], findings: &mut Vec<Finding>) {
    if phrases.iter().any(Vec::is_empty) {
        findings.push(Finding::Error(human_errors::user(
            format!(
                "Every term in the phrase for '{name}' is optional, so it expands to include the empty phrase — which nobody can say."
            ),
            &[
                "At least one word must be required: move it outside its '[...]' group, e.g. 'deploy [the] sentry' rather than '[deploy] [the] [sentry]'.",
            ],
        )));
    }

    if phrases.len() > EXPANSION_WARNING_THRESHOLD {
        findings.push(Finding::Warning(format!(
            "this command expands into {} concrete phrases (more than {EXPANSION_WARNING_THRESHOLD}), which makes the recognition grammar large and the command easier to trigger by accident",
            phrases.len()
        )));
    }
}

/// Lints the relations *between* commands: duplicated phrases and prefixes.
///
/// Both are computed from the expansions with plain set logic rather than from
/// the matcher's trie, so `validate` stays a pure function of the profile.
fn lint_phrase_relations(
    profile: &Profile,
    expansions: &[Vec<Vec<String>>],
    sections: &mut [Section],
) {
    let mut owner: HashMap<&[String], usize> = HashMap::new();
    let mut duplicates: Vec<(usize, usize, String)> = Vec::new();

    for (index, phrases) in expansions.iter().enumerate() {
        for phrase in phrases {
            match owner.get(phrase.as_slice()) {
                // A command's own expansion is already deduplicated, so a
                // repeat here is always a clash between two commands.
                Some(&first) => duplicates.push((first, index, phrase.join(" "))),
                None => {
                    owner.insert(phrase.as_slice(), index);
                }
            }
        }
    }

    for (first, second, phrase) in duplicates {
        let (a, b) = (
            profile.commands[first].display_name(),
            profile.commands[second].display_name(),
        );
        sections[second].findings.push(Finding::Warning(format!(
            "the phrase \"{phrase}\" is used by both '{a}' and '{b}', so only one of them can ever fire"
        )));
    }

    // A command whose full phrase is a strict word-prefix of another command's
    // phrase cannot fire immediately: the matcher has to wait and see whether
    // the longer phrase is still coming. Surfacing that here is what makes the
    // completion timeout discoverable per profile.
    let timeout = duration::render(profile.completion_timeout);
    let mut notes: Vec<(usize, String, String)> = Vec::new();
    let mut seen = HashSet::new();

    for phrases in expansions.iter() {
        for phrase in phrases {
            for length in 1..phrase.len() {
                let prefix = &phrase[..length];
                let Some(&short) = owner.get(prefix) else {
                    continue;
                };
                if owner.get(phrase.as_slice()) == Some(&short) {
                    // Two phrases of the same command: whichever wins, the
                    // same command fires, so there is nothing to warn about.
                    continue;
                }

                let (short_phrase, long_phrase) = (prefix.join(" "), phrase.join(" "));
                if seen.insert((short, short_phrase.clone(), long_phrase.clone())) {
                    notes.push((short, short_phrase, long_phrase));
                }
            }
        }
    }

    notes.sort();
    for (index, short, long) in notes {
        sections[index].findings.push(Finding::Note(format!(
            "saying \"{short}\" will wait {timeout} in case you continue with \"{long}\""
        )));
    }
}

/// Checks every word the profile listens for against the model's vocabulary.
///
/// Words are collected with [`expansion::word_set`] — a linear walk of the AST
/// — so a command with 512 expansions still only costs one lookup per distinct
/// word, and lookups are memoized across commands.
fn check_vocabulary(
    profile: &Profile,
    model: &Path,
    vocabulary: &mut dyn Vocabulary,
    sections: &mut [Section],
    profile_findings: &mut Vec<Finding>,
) {
    let candidates = vocabulary.words().map(|words| nearest_candidates(&words));
    let mut known: HashMap<String, bool> = HashMap::new();
    let mut saw_unknown = false;

    for (index, command) in profile.commands.iter().enumerate() {
        for word in expansion::word_set(command.phrase.expr()) {
            // Memoized across commands: a word shared by twenty commands costs
            // one lookup, but is still reported under each of them, because the
            // report is read one command at a time.
            let recognized = match known.get(&word) {
                Some(&recognized) => recognized,
                None => {
                    let recognized = vocabulary.contains(&word);
                    known.insert(word.clone(), recognized);
                    recognized
                }
            };

            if !recognized {
                saw_unknown = true;
                sections[index]
                    .findings
                    .push(unknown_word(model, &word, vocabulary, &candidates));
            }
        }
    }

    if saw_unknown && candidates.is_none() {
        profile_findings.push(Finding::Note(
            "this model does not ship a readable word list (<model>/graph/words.txt), so we cannot suggest the words it does know — only spelling fixes and compound splits".to_string(),
        ));
    }
}

/// Builds the "the model does not know this word" error, with suggestions.
fn unknown_word(
    model: &Path,
    word: &str,
    vocabulary: &mut dyn Vocabulary,
    candidates: &Option<Vec<String>>,
) -> Finding {
    let mut suggestions = Vec::new();

    // 1. Normalization: punctuation pasted into a phrase never reaches the
    //    recognizer, so a word which is only unknown *because* of it is really
    //    a spelling problem.
    let normalized: String = word
        .to_lowercase()
        .chars()
        .filter(|c| !STRIPPED_PUNCTUATION.contains(c))
        .collect();
    if normalized != word && !normalized.is_empty() && vocabulary.contains(&normalized) {
        suggestions.push(format!("'{normalized}'"));
    }

    // 2. Compound decomposition: "autocannon" is two words the model knows.
    suggestions.extend(
        compound_splits(word, vocabulary)
            .into_iter()
            .map(|(left, right)| format!("'{left} {right}'")),
    );

    // 3. Nearest known words, when the model tells us what it knows.
    if let Some(candidates) = candidates {
        suggestions.extend(
            nearest(word, candidates)
                .into_iter()
                .map(|w| format!("'{w}'")),
        );
    }

    let model = model.display();
    let message = if suggestions.is_empty() {
        format!(
            "The model at '{model}' does not know the word '{word}', so this command can never be recognized."
        )
    } else {
        format!(
            "The model at '{model}' does not know the word '{word}', so this command can never be recognized. Did you mean {}?",
            suggestions.join(", ")
        )
    };

    Finding::Error(human_errors::user(
        message,
        &[
            "Replace the word with one the model knows, or offer both spellings with an '{alternate, choices}' group, e.g. '{autocannon, auto cannon}'.",
            "A larger model knows more words — the model list at https://alphacephei.com/vosk/models shows what is available.",
        ],
    ))
}

/// Every way of splitting `word` into two words the model knows, most balanced
/// first, capped at [`MAX_SPLIT_SUGGESTIONS`].
fn compound_splits(word: &str, vocabulary: &mut dyn Vocabulary) -> Vec<(String, String)> {
    let mut splits: Vec<(usize, String, String)> = Vec::new();

    for (index, _) in word.char_indices().skip(1) {
        let (left, right) = word.split_at(index);
        if vocabulary.contains(left) && vocabulary.contains(right) {
            splits.push((left.len().abs_diff(right.len()), left.into(), right.into()));
        }
    }

    splits.sort();
    splits
        .into_iter()
        .take(MAX_SPLIT_SUGGESTIONS)
        .map(|(_, left, right)| (left, right))
        .collect()
}

/// Filters a model's raw symbol table down to things a person could say.
///
/// `words.txt` is an FST symbol table, so alongside real words it carries
/// `<eps>`, `<unk>`, `#0` and other machinery; suggesting those would be worse
/// than suggesting nothing.
fn nearest_candidates(words: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();

    for word in words {
        if word.contains(['<', '>', '#']) {
            continue;
        }

        let lowered = word.to_lowercase();
        if !lowered
            .chars()
            .all(|c| c.is_alphabetic() || c == '\'' || c == '-')
            || !lowered.chars().any(char::is_alphabetic)
        {
            continue;
        }

        if seen.insert(lowered.clone()) {
            candidates.push(lowered);
        }
    }

    candidates
}

/// The closest known words to `word` by Levenshtein distance, preferring a
/// shared first letter, capped at [`MAX_NEAREST_SUGGESTIONS`].
fn nearest(word: &str, candidates: &[String]) -> Vec<String> {
    let first = word.chars().next();

    let mut ranked: Vec<(usize, bool, usize, &String)> = candidates
        .iter()
        .filter(|candidate| candidate.as_str() != word)
        .filter_map(|candidate| {
            let distance = strsim::levenshtein(word, candidate);
            (distance <= MAX_SUGGESTION_DISTANCE).then_some((
                distance,
                // `false` sorts first, so a shared first letter wins ties.
                candidate.chars().next() != first,
                candidate.len().abs_diff(word.len()),
                candidate,
            ))
        })
        .collect();

    ranked.sort();
    ranked
        .into_iter()
        .take(MAX_NEAREST_SUGGESTIONS)
        .map(|(_, _, _, candidate)| candidate.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LoadedProfile;
    use rstest::rstest;

    /// A [`Vocabulary`] backed by a plain word set.
    #[derive(Default)]
    struct FakeVocabulary {
        words: HashSet<String>,
        /// What `words()` reports; `None` models a compiled-graph model which
        /// ships no readable symbol table.
        listing: Option<Vec<String>>,
    }

    impl FakeVocabulary {
        /// A vocabulary which knows `words` and will list them.
        fn new(words: &[&str]) -> Self {
            Self {
                words: words.iter().map(|w| w.to_string()).collect(),
                listing: Some(words.iter().map(|w| w.to_string()).collect()),
            }
        }

        /// The same vocabulary, but without a readable word list.
        fn without_word_list(mut self) -> Self {
            self.listing = None;
            self
        }

        /// The same vocabulary, listing extra FST symbols the way a real
        /// `words.txt` does.
        fn listing_symbols(mut self, symbols: &[&str]) -> Self {
            let listing = self.listing.get_or_insert_with(Vec::new);
            listing.extend(symbols.iter().map(|s| s.to_string()));
            self
        }
    }

    impl Vocabulary for FakeVocabulary {
        fn contains(&mut self, word: &str) -> bool {
            self.words.contains(word)
        }

        fn words(&self) -> Option<Vec<String>> {
            self.listing.clone()
        }
    }

    /// Every word the example-shaped profiles below use, so that a test which
    /// is not about the vocabulary never trips over it.
    fn full_vocabulary() -> FakeVocabulary {
        FakeVocabulary::new(&[
            "deploy", "the", "auto", "cannon", "sentry", "open", "terminal", "salute", "reload",
            "weapon", "a", "b", "c", "d", "x", "hold", "forward",
        ])
    }

    fn profile(yaml: &str) -> Profile {
        Profile::parse(&LoadedProfile {
            source: "test-profile.yaml".to_string(),
            content: yaml.to_string(),
        })
        .expect("the profile should load")
    }

    fn validate(yaml: &str, vocabulary: &mut dyn Vocabulary) -> Report {
        let profile = profile(yaml);
        let model = profile
            .model
            .clone()
            .unwrap_or_else(|| PathBuf::from("/models/en"));
        check("test-profile.yaml", &profile, &model, Ok(vocabulary))
    }

    /// A profile with `model:` and `commands:` wrapped around `commands`.
    fn with_commands(commands: &str) -> String {
        format!("model: /models/en\ncommands:\n{commands}")
    }

    #[test]
    fn test_a_clean_profile_reports_nothing() {
        let report = validate(
            &with_commands(
                "  - phrase: open [the] terminal\n    keys: [\"leftctrl+leftalt+t\"]\n  - phrase: salute\n    events:\n      - down: x\n      - wait: 750ms\n      - up: x\n",
            ),
            &mut full_vocabulary(),
        );

        let rendered = report.render();
        assert_eq!(report.errors(), 0, "unexpected errors:\n{rendered}");
        assert_eq!(report.warnings(), 0, "unexpected warnings:\n{rendered}");
        assert_eq!(report.exit_code(), 0);

        assert!(
            rendered.contains("open [the] terminal"),
            "every command gets a section:\n{rendered}"
        );
        assert!(
            rendered.contains("2 commands checked — 0 errors, 0 warnings."),
            "unexpected summary:\n{rendered}"
        );
    }

    #[test]
    fn test_the_header_names_the_source_and_the_profile() {
        let report = validate(
            &format!(
                "name: Deep Rock Galactic\n{}",
                with_commands("  - phrase: salute\n    keys: [\"x\"]\n")
            ),
            &mut full_vocabulary(),
        );

        assert!(
            report
                .render()
                .starts_with("test-profile.yaml — Deep Rock Galactic\n"),
            "unexpected report:\n{}",
            report.render()
        );
    }

    #[test]
    fn test_unknown_words_are_errors_under_their_command() {
        let mut vocabulary = FakeVocabulary::new(&["deploy", "the", "sentry"]);
        let report = validate(
            &with_commands(
                "  - name: Deploy\n    phrase: deploy [the] autocannon\n    keys: [\"4\"]\n",
            ),
            &mut vocabulary,
        );

        assert_eq!(report.exit_code(), 1);
        let messages = report.error_messages();
        assert_eq!(messages.len(), 1, "unexpected errors: {messages:?}");
        assert!(
            messages[0].contains("does not know the word 'autocannon'"),
            "unexpected error: {}",
            messages[0]
        );
        assert!(
            report.render().contains("Deploy"),
            "the finding belongs to its command's section:\n{}",
            report.render()
        );
    }

    #[test]
    fn test_a_compound_word_is_suggested_as_a_split() {
        // DESIGN.md's worked example: the model has never heard "autocannon",
        // but it knows both halves of it.
        let mut vocabulary = FakeVocabulary::new(&["deploy", "auto", "cannon", "a", "utocannon"])
            .without_word_list();
        let report = validate(
            &with_commands("  - phrase: deploy autocannon\n    keys: [\"4\"]\n"),
            &mut vocabulary,
        );

        let messages = report.error_messages();
        assert_eq!(messages.len(), 1, "unexpected errors: {messages:?}");
        // 'auto cannon' (4 vs 6) is more balanced than 'a utocannon' (1 vs 9),
        // so it leads.
        assert!(
            messages[0].contains("Did you mean 'auto cannon', 'a utocannon'?"),
            "unexpected error: {}",
            messages[0]
        );
    }

    #[test]
    fn test_at_most_two_splits_are_offered() {
        // "abcd" splits three ways; the model knows the halves of two of them.
        let mut vocabulary =
            FakeVocabulary::new(&["a", "b", "c", "d", "ab", "cd", "abc"]).without_word_list();
        let report = validate(
            &with_commands("  - phrase: abcd\n    keys: [\"x\"]\n"),
            &mut vocabulary,
        );

        let message = report.error_messages().remove(0);
        assert!(
            message.contains("Did you mean 'ab cd', 'abc d'?"),
            "the most balanced split should lead, and only two should be offered: {message}"
        );
    }

    #[test]
    fn test_punctuation_is_normalized_away() {
        let mut vocabulary = FakeVocabulary::new(&["dont", "deploy"]);
        // The DSL allows apostrophes inside words, so this reaches the
        // vocabulary check intact.
        let report = validate(
            &with_commands("  - phrase: deploy don't\n    keys: [\"x\"]\n"),
            &mut vocabulary,
        );

        let message = report.error_messages().remove(0);
        assert!(
            message.contains("does not know the word 'don't'") && message.contains("'dont'"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn test_nearest_words_are_ranked_and_capped() {
        // Every one of these is one edit away from "bat", so the ranking has
        // nothing to go on but the shared first letter and the alphabet — and
        // only three of the six may be offered.
        let mut vocabulary = FakeVocabulary::new(&["cat", "bad", "bar", "bag", "bit", "hat"]);
        let report = validate(
            &with_commands("  - phrase: bat\n    keys: [\"x\"]\n"),
            &mut vocabulary,
        );

        let message = report.error_messages().remove(0);
        assert!(
            message.contains("Did you mean 'bad', 'bag', 'bar'?"),
            "a shared first letter should win ties, capped at {MAX_NEAREST_SUGGESTIONS}: {message}"
        );
    }

    #[test]
    fn test_distant_words_are_never_suggested() {
        let mut vocabulary = FakeVocabulary::new(&["deploy", "sentry"]);
        let report = validate(
            &with_commands("  - phrase: gubbins\n    keys: [\"x\"]\n"),
            &mut vocabulary,
        );

        let message = report.error_messages().remove(0);
        assert!(
            !message.contains("Did you mean"),
            "nothing is within {MAX_SUGGESTION_DISTANCE} edits, so we should stay quiet: {message}"
        );
    }

    #[test]
    fn test_fst_symbols_are_never_suggested() {
        let mut vocabulary = FakeVocabulary::new(&["deploy", "sentry"])
            .listing_symbols(&["<eps>", "<unk>", "#0", "!SIL", "1234"]);
        let report = validate(
            &with_commands("  - phrase: deploy sentrz\n    keys: [\"x\"]\n"),
            &mut vocabulary,
        );

        let message = report.error_messages().remove(0);
        for symbol in ["<eps>", "<unk>", "#0", "!SIL", "1234"] {
            assert!(
                !message.contains(symbol),
                "'{symbol}' is not something a person can say: {message}"
            );
        }
        assert!(message.contains("'sentry'"), "unexpected error: {message}");
    }

    #[test]
    fn test_a_model_without_a_word_list_says_so_once() {
        let mut vocabulary = FakeVocabulary::new(&["deploy"]).without_word_list();
        let report = validate(
            &with_commands(
                "  - phrase: deploy sentrz\n    keys: [\"x\"]\n  - phrase: deploy gubbins\n    keys: [\"y\"]\n",
            ),
            &mut vocabulary,
        );

        let rendered = report.render();
        assert_eq!(
            rendered
                .matches("does not ship a readable word list")
                .count(),
            1,
            "the note belongs to the profile, not to every word:\n{rendered}"
        );
        assert_eq!(report.errors(), 2, "both words are still reported");
    }

    #[test]
    fn test_a_known_profile_never_mentions_the_missing_word_list() {
        let report = validate(
            &with_commands("  - phrase: salute\n    keys: [\"x\"]\n"),
            &mut full_vocabulary().without_word_list(),
        );

        assert!(
            !report.render().contains("readable word list"),
            "there is nothing to suggest, so nothing to apologise for"
        );
    }

    #[test]
    fn test_a_missing_model_does_not_hide_the_grammar_lints() {
        let profile = profile(&with_commands(
            "  - name: Broken\n    phrase: \"[deploy] [the] [sentry]\"\n    keys: [\"x\"]\n",
        ));
        let report = check(
            "test-profile.yaml",
            &profile,
            Path::new("/models/en"),
            Err(human_errors::user(
                "We could not find a Vosk model at '/models/en'.",
                &["Check the path."],
            )),
        );

        let rendered = report.render();
        assert!(
            rendered.contains("could not find a Vosk model"),
            "the model failure is reported:\n{rendered}"
        );
        assert!(
            rendered.contains("Every term in the phrase for 'Broken' is optional"),
            "and so is everything it could have hidden:\n{rendered}"
        );
        assert_eq!(report.errors(), 2);
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn test_an_all_optional_phrase_is_an_error() {
        let report = validate(
            &with_commands("  - name: Ghost\n    phrase: \"[deploy] [the]\"\n    keys: [\"x\"]\n"),
            &mut full_vocabulary(),
        );

        let message = report.error_messages().remove(0);
        assert!(
            message.contains("'Ghost'") && message.contains("empty phrase"),
            "unexpected error: {message}"
        );
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn test_duplicate_phrases_warn_and_name_both_commands() {
        let report = validate(
            &with_commands(
                "  - name: First\n    phrase: salute\n    keys: [\"x\"]\n  - name: Second\n    phrase: \"[the] salute\"\n    keys: [\"a\"]\n",
            ),
            &mut full_vocabulary(),
        );

        let rendered = report.render();
        assert!(
            rendered.contains("the phrase \"salute\" is used by both 'First' and 'Second'"),
            "unexpected report:\n{rendered}"
        );
        assert_eq!(report.warnings(), 1);
        assert_eq!(
            report.exit_code(),
            0,
            "a duplicate is a warning here and an error in `run`"
        );
    }

    #[test]
    fn test_prefix_relations_are_noted_with_the_timeout() {
        let report = validate(
            "model: /models/en\ncompletion_timeout: 350ms\ncommands:\n  - name: Reload\n    phrase: reload\n    keys: [\"x\"]\n  - name: Reload weapon\n    phrase: reload weapon\n    keys: [\"a\"]\n",
            &mut full_vocabulary(),
        );

        let rendered = report.render();
        assert!(
            rendered.contains(
                "note: saying \"reload\" will wait 350ms in case you continue with \"reload weapon\""
            ),
            "unexpected report:\n{rendered}"
        );
        assert_eq!(report.errors(), 0);
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn test_prefixes_within_one_command_are_not_noted() {
        // "deploy the sentry" is a prefix of nothing else, but "deploy" is a
        // prefix of "deploy the sentry" *within the same command* — whichever
        // wins, the same keys are pressed.
        let report = validate(
            &with_commands("  - phrase: deploy [the sentry]\n    keys: [\"x\"]\n"),
            &mut full_vocabulary(),
        );

        assert!(
            !report.render().contains("will wait"),
            "unexpected report:\n{}",
            report.render()
        );
    }

    #[test]
    fn test_a_large_expansion_warns() {
        // Eight chained two-way alternates expand to 256 phrases: within the
        // hard cap of 512, but past the warning threshold of 128.
        let phrase = ["{a, b}"; 8].join(" ");
        let report = validate(
            &with_commands(&format!(
                "  - name: Sprawling\n    phrase: \"{phrase}\"\n    keys: [\"x\"]\n"
            )),
            &mut full_vocabulary(),
        );

        let rendered = report.render();
        assert!(
            rendered.contains("expands into 256 concrete phrases"),
            "unexpected report:\n{rendered}"
        );
        assert_eq!(report.warnings(), 1);
        assert_eq!(report.exit_code(), 0, "a large phrase still works");
    }

    #[test]
    fn test_an_expansion_below_the_threshold_is_quiet() {
        let phrase = ["{a, b}"; 7].join(" ");
        let report = validate(
            &with_commands(&format!("  - phrase: \"{phrase}\"\n    keys: [\"x\"]\n")),
            &mut full_vocabulary(),
        );

        assert_eq!(
            report.warnings(),
            0,
            "128 phrases is fine:\n{}",
            report.render()
        );
    }

    #[test]
    fn test_an_unreleased_key_is_a_note() {
        let report = validate(
            &with_commands(
                "  - name: Hold forward\n    phrase: hold forward\n    events:\n      - down: w\n",
            ),
            &mut FakeVocabulary::new(&["hold", "forward"]),
        );

        let rendered = report.render();
        assert!(
            rendered.contains("note: this command holds 'w' down and never releases it"),
            "unexpected report:\n{rendered}"
        );
        assert_eq!(report.errors(), 0);
        assert_eq!(report.warnings(), 0, "hold macros are legal");
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn test_an_unmatched_release_is_a_warning() {
        let report = validate(
            &with_commands(
                "  - name: Slip\n    phrase: salute\n    events:\n      - up: w\n      - down: x\n      - up: x\n",
            ),
            &mut full_vocabulary(),
        );

        let rendered = report.render();
        assert!(
            rendered.contains("warning: this command releases 'w' without ever pressing it"),
            "unexpected report:\n{rendered}"
        );
        assert_eq!(report.warnings(), 1);
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn test_the_keys_shorthand_is_always_balanced() {
        let report = validate(
            &with_commands("  - phrase: salute\n    keys: [\"leftctrl+x\"]\n"),
            &mut full_vocabulary(),
        );

        assert_eq!(
            report.warnings() + report.errors(),
            0,
            "the shorthand cannot be unbalanced:\n{}",
            report.render()
        );
        assert!(!report.render().contains("note:"));
    }

    #[test]
    fn test_everything_is_reported_in_one_pass() {
        let report = validate(
            &with_commands(
                "  - name: Ghost\n    phrase: \"[deploy] [the]\"\n    keys: [\"x\"]\n  - name: Slip\n    phrase: gubbins\n    events:\n      - up: w\n",
            ),
            &mut full_vocabulary(),
        );

        let rendered = report.render();
        // The empty-phrase error, the unknown word, and the unmatched release
        // all appear together.
        assert!(rendered.contains("empty phrase"), "{rendered}");
        assert!(rendered.contains("'gubbins'"), "{rendered}");
        assert!(rendered.contains("without ever pressing it"), "{rendered}");
        assert_eq!(report.errors(), 2);
        assert_eq!(report.warnings(), 1);
        assert!(
            rendered.contains("2 commands checked — 2 errors, 1 warning."),
            "unexpected summary:\n{rendered}"
        );
    }

    #[rstest]
    #[case(0, 0, 0)]
    #[case(0, 3, 0)]
    #[case(1, 0, 1)]
    #[case(2, 5, 1)]
    fn test_exit_code_semantics(
        #[case] errors: usize,
        #[case] warnings: usize,
        #[case] expected: i32,
    ) {
        let mut findings = Vec::new();
        for _ in 0..errors {
            findings.push(Finding::Error(human_errors::user("broken", &["fix it"])));
        }
        for _ in 0..warnings {
            findings.push(Finding::Warning("odd".to_string()));
        }

        let report = Report {
            header: "test-profile.yaml".to_string(),
            profile: Vec::new(),
            commands: vec![Section {
                title: "a command".to_string(),
                findings,
            }],
        };

        assert_eq!(report.errors(), errors);
        assert_eq!(report.warnings(), warnings);
        assert_eq!(report.exit_code(), expected);
    }

    #[test]
    fn test_clean_commands_render_as_ok() {
        let report = validate(
            &with_commands("  - phrase: salute\n    keys: [\"x\"]\n"),
            &mut full_vocabulary(),
        );

        assert!(
            report.render().contains("salute\n  ok\n"),
            "unexpected report:\n{}",
            report.render()
        );
    }

    fn model_path() -> std::path::PathBuf {
        std::env::var_os("VOSK_MODEL_PATH").map_or_else(
            || {
                std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME must be set"))
                    .join(".cache/vosk/vosk-model-small-en-us-0.15")
            },
            std::path::PathBuf::from,
        )
    }

    #[test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    fn real_model_validates_the_example_profile() {
        let path = model_path();
        assert!(
            path.is_dir(),
            "no Vosk model at '{}' — download one from https://alphacephei.com/vosk/models and set VOSK_MODEL_PATH, or run with --features pure_tests to skip this test",
            path.display()
        );

        let profile = Profile::parse(&LoadedProfile {
            source: "examples/profile.yaml".to_string(),
            content: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/examples/profile.yaml"
            ))
            .to_string(),
        })
        .expect("the example profile should load");

        // The example points at wherever the author keeps their models; the
        // test points at wherever *this* machine keeps them, which is exactly
        // the override `--model` gives a person running `validate`.
        let path = crate::config::resolve_model(Some(&path), &profile, &SystemConfig::default())
            .expect("the override should resolve");

        let mut vocabulary = VoskVocabulary::open(&path).expect("the model should open");
        let knows_autocannon = vocabulary.contains("autocannon");

        let report = check(
            "examples/profile.yaml",
            &profile,
            &path,
            Ok(&mut vocabulary),
        );
        let rendered = report.render();

        for command in &profile.commands {
            assert!(
                rendered.contains(command.display_name()),
                "'{}' should have a section:\n{rendered}",
                command.display_name()
            );
        }

        // Nothing structural, nothing about the grammar, and no unbalanced
        // output: the small English model's vocabulary is the only thing which
        // may have something to say.
        for message in report.error_messages() {
            assert!(
                message.contains("does not know the word"),
                "the example profile should only ever trip on vocabulary:\n{rendered}"
            );
        }
        assert_eq!(report.warnings(), 0, "unexpected warnings:\n{rendered}");

        if knows_autocannon {
            assert_eq!(report.exit_code(), 0, "unexpected report:\n{rendered}");
        } else {
            // The DESIGN.md worked example: the model does not know
            // "autocannon", but it knows both halves of it.
            assert!(
                rendered.contains("'auto cannon'"),
                "the compound split should be suggested:\n{rendered}"
            );
            assert_eq!(report.exit_code(), 1);
        }
    }
}
