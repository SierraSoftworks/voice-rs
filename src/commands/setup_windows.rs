//! `voice-orders setup` on Windows: there is nothing to set up.
//!
//! The Linux command exists because kernel-level input needs one-time system
//! configuration — a udev rule, the `uinput` module, membership of the `input`
//! group — and none of that has a Windows equivalent. `SendInput` is an
//! ordinary user-space API available to every process, and the low-level
//! keyboard hook the listen hotkey uses needs no driver and no elevation
//! either. So the command stays, says so, and exits successfully: a setup
//! script which runs `voice-orders setup` on both platforms should not have to
//! know which one it is on.

use clap::Args;

/// What this command tells a Windows user.
const NOTHING_TO_CONFIGURE: &str = "Nothing to configure: Windows needs no drivers or permissions \
for voice-orders. Keyboard output goes through SendInput and the listen hotkey through a \
low-level keyboard hook, both of which are ordinary user-space APIs. Run 'voice-orders doctor' \
to check the rest of your installation.";

#[derive(Args, Debug)]
pub struct SetupArgs {
    /// Apply the changes without asking for confirmation. There are none on
    /// Windows, so this has no effect.
    #[arg(long)]
    pub yes: bool,

    /// Print the equivalent shell commands and exit without changing anything.
    /// There are none on Windows, so this has no effect.
    #[arg(long)]
    pub print: bool,
}

/// Reports that this machine needs no configuration, returning the exit code
/// to leave with.
pub async fn run(args: SetupArgs) -> Result<i32, crate::Error> {
    // Both flags are accepted so the CLI surface matches Linux's, and both are
    // no-ops: there is no plan to confirm and no plan to print.
    let _ = (args.yes, args.print);

    println!("{NOTHING_TO_CONFIGURE}");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_explains_why_there_is_nothing_to_do() {
        assert!(NOTHING_TO_CONFIGURE.contains("Nothing to configure"));
        assert!(
            NOTHING_TO_CONFIGURE.contains("SendInput"),
            "the message should say what replaces uinput"
        );
    }

    #[tokio::test]
    async fn setup_succeeds_with_either_flag() {
        for (yes, print) in [(false, false), (true, false), (false, true)] {
            assert_eq!(
                run(SetupArgs { yes, print }).await.unwrap(),
                0,
                "setup should succeed with yes={yes}, print={print}"
            );
        }
    }
}
