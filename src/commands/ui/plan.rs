//! Rendering a compiled output plan the way a person reads a macro.
//!
//! Shared by both commands because it says the same thing in both: `test`
//! reports the plan it *would* have played, `run` reports the one it *is*
//! playing, and a plan reads the same either way — which is what lets one
//! wording (`"deploy the autocannon" -> Autocannon (leftctrl+4)`) be true of
//! both.

use crate::output::{CompiledOutput, KeyCode, KeyEvent};

/// Renders a compiled output plan the way a person reads a macro.
///
/// Keys pressed together come back as a `+`-joined chord, waits are elided (the
/// hold and interval timings are a `run` concern, not a "did I say the right
/// thing?" one), and the two unbalanced cases a profile is allowed to contain
/// are called out rather than silently dropped: a key which is never released
/// (a hold-style macro) and a release with no press before it.
pub(crate) fn render_plan(output: &CompiledOutput) -> String {
    let CompiledOutput::Keyboard(plan) = output;

    let mut steps: Vec<String> = Vec::new();
    // The keys of the chord being assembled, in the order they were pressed,
    // and how many of them are still held down.
    let mut chord: Vec<KeyCode> = Vec::new();
    let mut holding = 0usize;

    for event in plan {
        match *event {
            KeyEvent::Down(key) => {
                if !chord.contains(&key) {
                    chord.push(key);
                    holding += 1;
                }
            }
            KeyEvent::Up(key) => {
                if !chord.contains(&key) {
                    steps.push(format!("(release {key})"));
                    continue;
                }

                holding -= 1;
                // The chord is only finished once every key in it is back up.
                if holding == 0 {
                    steps.push(render_chord(&chord));
                    chord.clear();
                }
            }
            KeyEvent::ReleaseAll => {
                // Everything outstanding goes up at once, so whatever chord was
                // being assembled ends here whether or not it was balanced.
                if !chord.is_empty() {
                    steps.push(render_chord(&chord));
                    chord.clear();
                    holding = 0;
                }

                steps.push("(release everything)".to_string());
            }
            KeyEvent::Wait(_) => {}
        }
    }

    if !chord.is_empty() {
        steps.push(format!("{} (held)", render_chord(&chord)));
    }

    if steps.is_empty() {
        return "(nothing)".to_string();
    }

    steps.join(" ")
}

/// One chord: its key names joined by `+`, in the order they go down.
fn render_chord(keys: &[KeyCode]) -> String {
    keys.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("+")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutputDefaults;
    use crate::grammar::{Automaton, Grammar};
    use crate::output::assembly::assemble;
    use crate::output::keys;
    use rstest::rstest;
    use std::time::Duration;

    fn key(name: &str) -> KeyCode {
        keys::from_name(name).expect("a known key")
    }

    /// Compiles one action block the way the pipeline does — grammar →
    /// automaton → walk → assemble — so the rendering is asserted against real
    /// assembled plans rather than hand-built ones.
    fn plan(actions: &str) -> CompiledOutput {
        let source = format!("Salute = \"salute\" {actions}");
        let grammar = Grammar::parse(&source).expect("the grammar should parse");
        let automaton = Automaton::compile(&grammar).expect("the grammar should compile");
        let mut walk = automaton.walk();
        walk.step("salute");
        let accepts = walk.accepts();
        assert_eq!(accepts.len(), 1, "one reading expected: {accepts:?}");
        CompiledOutput::Keyboard(assemble(&accepts[0].actions, &OutputDefaults::default()))
    }

    #[rstest]
    // A single key: the hold wait is elided.
    #[case("{ 4 }", "4")]
    // A chord: pressed in order, released in reverse, reported as one step.
    #[case("{ leftctrl+leftalt+t }", "leftctrl+leftalt+t")]
    // A sequence: the inter-press interval is elided too.
    #[case("{ a, b }", "a b")]
    #[case("{ leftshift+a, b }", "leftshift+a b")]
    // Explicit hold/wait/release, including its long hold.
    #[case("{ hold(x), wait(750ms), release(x) }", "x")]
    // A hold-style macro: legal, and worth saying out loud.
    #[case("{ hold(w) }", "w (held)")]
    // A release with no press before it: also legal, also worth saying.
    #[case("{ release(w), x }", "(release w) x")]
    fn test_render_plan(#[case] actions: &str, #[case] expected: &str) {
        assert_eq!(render_plan(&plan(actions)), expected);
    }

    #[test]
    fn test_an_empty_plan_says_so() {
        assert_eq!(
            render_plan(&CompiledOutput::Keyboard(Vec::new())),
            "(nothing)"
        );
    }

    #[test]
    fn test_waits_alone_are_elided_entirely() {
        assert_eq!(
            render_plan(&CompiledOutput::Keyboard(vec![KeyEvent::Wait(
                Duration::from_secs(1)
            )])),
            "(nothing)"
        );
    }

    #[test]
    fn test_a_repeated_press_does_not_break_the_chord() {
        // Nothing in the schema produces this, but the renderer must not
        // underflow its hold count if something ever does.
        assert_eq!(
            render_plan(&CompiledOutput::Keyboard(vec![
                KeyEvent::Down(key("x")),
                KeyEvent::Down(key("x")),
                KeyEvent::Up(key("x")),
            ])),
            "x"
        );
    }

    #[test]
    fn test_release_everything_closes_the_chord_it_lets_go_of() {
        assert_eq!(
            render_plan(&CompiledOutput::Keyboard(vec![
                KeyEvent::Down(key("w")),
                KeyEvent::ReleaseAll,
            ])),
            "w (release everything)"
        );
        assert_eq!(
            render_plan(&CompiledOutput::Keyboard(vec![KeyEvent::ReleaseAll])),
            "(release everything)",
            "a panic command holds nothing of its own, and still says what it does"
        );
    }

    #[test]
    fn test_the_defaults_do_not_leak_into_the_rendering() {
        // The timings a plan assembles with are a `run` concern; a rehearsal
        // is about which keys, in which order.
        assert_eq!(
            OutputDefaults::default().duration,
            Duration::from_millis(30)
        );
        assert_eq!(render_plan(&plan("{ a }")), "a");
    }
}
