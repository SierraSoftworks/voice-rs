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

use crate::output::KeyCode;

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
