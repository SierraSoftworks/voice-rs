//! Turning a matched command's action program into a key-event plan.
//!
//! This is the vocabulary the grammar and the executor share: the matcher
//! hands over a flat list of [`ActionItem`]s — the action block with every
//! splice already expanded — and [`assemble`] applies the profile's pacing to
//! produce the [`KeyEvent`] plan the executor plays (DESIGN.md §"Command
//! semantics").
//!
//! Pacing lives here rather than in the matcher because it is a property of the
//! keyboard, not of the grammar: a game needs a press to be *held* long enough
//! to register and needs consecutive presses to be *separated* enough to be
//! counted twice. It is the same rule the `keys:` shorthand has always
//! followed, generalized to a stream which can also hold and release.

use std::time::Duration;

use crate::output::{KeyCode, KeyEvent};

/// The pacing an assembled plan is played at.
///
/// The profile's own `defaults:` block rather than a copy of it: the grammar's
/// pacing rules *are* the `keys:` shorthand's rules, and two structs which must
/// never disagree are better off being one struct.
pub type Pacing = crate::config::OutputDefaults;

/// One step of a matched command's action program, with splices expanded.
///
/// Abstract in the sense that it says what the command *means* — "press this
/// chord" — and leaves how long a press lasts to [`assemble`]. A `Press` is the
/// paired-edge form (down, hold, up); `Hold` and `Release` are the halves of it
/// a command spells out when the two edges belong to different utterances.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionItem {
    /// Press a chord and let it go: `m`, `shift+f1`.
    Press(Vec<KeyCode>),
    /// Press a chord and leave it down: `hold(w)`.
    Hold(Vec<KeyCode>),
    /// Let a chord back up: `release(w)`.
    Release(Vec<KeyCode>),
    /// Let go of everything the keyboard is holding: `release(*)`.
    ReleaseAll,
    /// Pause for exactly this long: `wait(20ms)`.
    Wait(Duration),
}

/// Applies `pacing` to an action program, producing a plan the executor plays.
///
/// The rules, in full:
///
/// - a `Press` puts every key down in the order it was written, holds the chord
///   for `pacing.duration`, then lifts them in reverse order so a modifier
///   outlives the key it modifies;
/// - consecutive presses are separated by `pacing.interval`, which is what
///   stops a game from reading two presses of the same key as one. Nothing
///   trails the last press: a command should not leave the executor idling once
///   its work is done;
/// - an explicit [`ActionItem::Wait`] **replaces** that interval rather than
///   adding to it, so `press, wait(20ms), press` is exactly 20ms apart — the
///   author asked for a specific gap and gets it;
/// - `Hold`, `Release` and `ReleaseAll` are immediate. They carry no pacing of
///   their own, and because the interval belongs *between presses*, one of them
///   standing between two presses means those presses are no longer
///   consecutive: their timing is then the author's to state with `wait`.
pub fn assemble(items: &[ActionItem], pacing: &Pacing) -> Vec<KeyEvent> {
    let mut plan = Vec::new();
    // Whether the item we just emitted was a press, and so whether the next
    // press needs the interval put in front of it.
    let mut after_press = false;

    for item in items {
        // Nothing the grammar can write produces an empty chord, but one must
        // be a true no-op rather than a bare hold-duration wait.
        if matches!(
            item,
            ActionItem::Press(chord) | ActionItem::Hold(chord) | ActionItem::Release(chord)
                if chord.is_empty()
        ) {
            continue;
        }

        match item {
            ActionItem::Press(chord) => {
                if after_press {
                    plan.push(KeyEvent::Wait(pacing.interval));
                }

                plan.extend(chord.iter().map(|key| KeyEvent::Down(*key)));
                plan.push(KeyEvent::Wait(pacing.duration));
                plan.extend(chord.iter().rev().map(|key| KeyEvent::Up(*key)));

                after_press = true;
                continue;
            }
            ActionItem::Hold(chord) => {
                plan.extend(chord.iter().map(|key| KeyEvent::Down(*key)));
            }
            ActionItem::Release(chord) => {
                plan.extend(chord.iter().rev().map(|key| KeyEvent::Up(*key)));
            }
            ActionItem::ReleaseAll => plan.push(KeyEvent::ReleaseAll),
            ActionItem::Wait(duration) => plan.push(KeyEvent::Wait(*duration)),
        }

        after_press = false;
    }

    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::keys;
    use rstest::rstest;

    fn key(name: &str) -> KeyCode {
        keys::from_name(name).expect("a known key")
    }

    fn pacing() -> Pacing {
        Pacing {
            duration: Duration::from_millis(30),
            interval: Duration::from_millis(25),
        }
    }

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    #[rstest]
    // Nothing in, nothing out.
    #[case(vec![], vec![])]
    // A single press: down, held for `duration`, up.
    #[case(
        vec![ActionItem::Press(vec![key("m")])],
        vec![KeyEvent::Down(key("m")), KeyEvent::Wait(ms(30)), KeyEvent::Up(key("m"))]
    )]
    // A chord goes down in order and comes up in reverse.
    #[case(
        vec![ActionItem::Press(vec![key("leftshift"), key("f1")])],
        vec![
            KeyEvent::Down(key("leftshift")),
            KeyEvent::Down(key("f1")),
            KeyEvent::Wait(ms(30)),
            KeyEvent::Up(key("f1")),
            KeyEvent::Up(key("leftshift")),
        ]
    )]
    // Consecutive presses are separated by the interval, and nothing trails
    // the last one.
    #[case(
        vec![
            ActionItem::Press(vec![key("1")]),
            ActionItem::Press(vec![key("2")]),
            ActionItem::Press(vec![key("3")]),
        ],
        vec![
            KeyEvent::Down(key("1")), KeyEvent::Wait(ms(30)), KeyEvent::Up(key("1")),
            KeyEvent::Wait(ms(25)),
            KeyEvent::Down(key("2")), KeyEvent::Wait(ms(30)), KeyEvent::Up(key("2")),
            KeyEvent::Wait(ms(25)),
            KeyEvent::Down(key("3")), KeyEvent::Wait(ms(30)), KeyEvent::Up(key("3")),
        ]
    )]
    // An explicit wait replaces the interval: 20ms between the presses, not
    // 25ms + 20ms.
    #[case(
        vec![
            ActionItem::Press(vec![key("1")]),
            ActionItem::Wait(ms(20)),
            ActionItem::Press(vec![key("2")]),
        ],
        vec![
            KeyEvent::Down(key("1")), KeyEvent::Wait(ms(30)), KeyEvent::Up(key("1")),
            KeyEvent::Wait(ms(20)),
            KeyEvent::Down(key("2")), KeyEvent::Wait(ms(30)), KeyEvent::Up(key("2")),
        ]
    )]
    // A wait before any press, and a wait with nothing after it, are both kept
    // exactly as written.
    #[case(
        vec![
            ActionItem::Wait(ms(500)),
            ActionItem::Press(vec![key("1")]),
            ActionItem::Wait(ms(500)),
        ],
        vec![
            KeyEvent::Wait(ms(500)),
            KeyEvent::Down(key("1")), KeyEvent::Wait(ms(30)), KeyEvent::Up(key("1")),
            KeyEvent::Wait(ms(500)),
        ]
    )]
    // hold ... press ... release: the hold and the release are immediate, and
    // the press between them still gets its hold duration.
    #[case(
        vec![
            ActionItem::Hold(vec![key("leftshift"), key("w")]),
            ActionItem::Press(vec![key("1")]),
            ActionItem::Release(vec![key("leftshift"), key("w")]),
        ],
        vec![
            KeyEvent::Down(key("leftshift")),
            KeyEvent::Down(key("w")),
            KeyEvent::Down(key("1")), KeyEvent::Wait(ms(30)), KeyEvent::Up(key("1")),
            KeyEvent::Up(key("w")),
            KeyEvent::Up(key("leftshift")),
        ]
    )]
    // An immediate item between two presses means they are no longer
    // consecutive, so no interval is inserted around it.
    #[case(
        vec![
            ActionItem::Press(vec![key("1")]),
            ActionItem::Hold(vec![key("w")]),
            ActionItem::Press(vec![key("2")]),
        ],
        vec![
            KeyEvent::Down(key("1")), KeyEvent::Wait(ms(30)), KeyEvent::Up(key("1")),
            KeyEvent::Down(key("w")),
            KeyEvent::Down(key("2")), KeyEvent::Wait(ms(30)), KeyEvent::Up(key("2")),
        ]
    )]
    // release(*) is one event, and pacing does not apply to it either.
    #[case(
        vec![
            ActionItem::Press(vec![key("1")]),
            ActionItem::ReleaseAll,
            ActionItem::Press(vec![key("2")]),
        ],
        vec![
            KeyEvent::Down(key("1")), KeyEvent::Wait(ms(30)), KeyEvent::Up(key("1")),
            KeyEvent::ReleaseAll,
            KeyEvent::Down(key("2")), KeyEvent::Wait(ms(30)), KeyEvent::Up(key("2")),
        ]
    )]
    // A panic command is nothing but the release.
    #[case(vec![ActionItem::ReleaseAll], vec![KeyEvent::ReleaseAll])]
    // An empty chord contributes nothing at all — not even a stray wait, and
    // not a break in the press adjacency around it.
    #[case(
        vec![
            ActionItem::Press(vec![key("1")]),
            ActionItem::Press(vec![]),
            ActionItem::Press(vec![key("2")]),
        ],
        vec![
            KeyEvent::Down(key("1")), KeyEvent::Wait(ms(30)), KeyEvent::Up(key("1")),
            KeyEvent::Wait(ms(25)),
            KeyEvent::Down(key("2")), KeyEvent::Wait(ms(30)), KeyEvent::Up(key("2")),
        ]
    )]
    fn test_assemble(#[case] items: Vec<ActionItem>, #[case] expected: Vec<KeyEvent>) {
        assert_eq!(assemble(&items, &pacing()), expected);
    }

    #[test]
    fn test_a_press_matches_the_old_keys_shorthand() {
        // The pacing rules are exactly the v1 `keys:` shorthand's, and this
        // pins the byte-for-byte plan that shorthand used to compile for the
        // same chords: down in written order, hold, up in reverse, an interval
        // between chords, nothing trailing the last one.
        assert_eq!(
            assemble(
                &[
                    ActionItem::Press(vec![key("leftctrl"), key("leftalt"), key("t")]),
                    ActionItem::Press(vec![key("4")]),
                ],
                &pacing()
            ),
            vec![
                KeyEvent::Down(key("leftctrl")),
                KeyEvent::Down(key("leftalt")),
                KeyEvent::Down(key("t")),
                KeyEvent::Wait(ms(30)),
                KeyEvent::Up(key("t")),
                KeyEvent::Up(key("leftalt")),
                KeyEvent::Up(key("leftctrl")),
                KeyEvent::Wait(ms(25)),
                KeyEvent::Down(key("4")),
                KeyEvent::Wait(ms(30)),
                KeyEvent::Up(key("4")),
            ]
        );
    }
}
