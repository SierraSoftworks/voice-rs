//! `voice-orders setup [--yes] [--print]`: applies the system configuration
//! `doctor` checks for. See DESIGN.md §"`setup` and `doctor`".
//!
//! The command is built around three rules, in order of importance:
//!
//! 1. **Diagnose before changing anything.** The plan is derived from the same
//!    checks [`doctor`] runs, so a machine which is half configured only ever
//!    gets the missing half, and running `setup` twice is a no-op.
//! 2. **Say exactly what will happen.** The plan — file paths, file contents,
//!    commands — is printed before the confirmation prompt, and `--print`
//!    stops there with the equivalent shell commands, for people who would
//!    rather apply them themselves or fold them into a machine setup script.
//! 3. **Elevate per step, never overall.** voice-orders must not run as root
//!    (it types into your session), so each individual step is spawned through
//!    `sudo` with stdio inherited, which is what lets sudo prompt for a
//!    password the way it always does.
//!
//! Everything above the actual spawning is a pure function of the filesystem
//! state and the user's name, which is what makes it testable without ever
//! running a privileged command.

use std::path::{Path, PathBuf};

use clap::Args;
use tokio::io::AsyncWriteExt;
use tracing_batteries::prelude::*;

use super::doctor::{self, GROUP_FILE, INPUT_GROUP, UINPUT_PATH};

/// The udev rule file we own. The `60-` prefix puts it after the distribution's
/// own input rules, so ours is the one which wins.
const UDEV_RULE_PATH: &str = "/etc/udev/rules.d/60-voice-orders-uinput.rules";

/// The rule which hands `/dev/uinput` to the `input` group. Kept byte-identical
/// to the one the uinput error message tells people to write by hand.
const UDEV_RULE: &str = r#"KERNEL=="uinput", GROUP="input", MODE="0660""#;

/// The `modules-load.d` file which loads `uinput` at every boot.
const MODULES_CONF_PATH: &str = "/etc/modules-load.d/voice-orders.conf";

/// The kernel module which provides `/dev/uinput`, and the whole content of
/// [`MODULES_CONF_PATH`].
const UINPUT_MODULE: &str = "uinput";

/// Advice for a step which failed while we were applying it.
const STEP_FAILURE_ADVICE: &[&str] = &[
    "Run 'voice-orders setup --print' to see the commands, and apply the failing one yourself to see what your system says about it.",
    "See the permissions guide at https://sierrasoftworks.github.io/voice-rs/guide/permissions.html for what each step does and why.",
];

#[derive(Args, Debug)]
pub struct SetupArgs {
    /// Apply the changes without asking for confirmation.
    #[arg(long)]
    pub yes: bool,

    /// Print the equivalent shell commands and exit without changing anything.
    #[arg(long)]
    pub print: bool,
}

/// The files `setup` reads and writes.
///
/// They are a parameter rather than constants so that the plan derivation can
/// be exercised against a temporary directory: a test must never be one typo
/// away from writing to the real `/etc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Paths {
    /// The udev rule file we write.
    pub udev_rule: PathBuf,
    /// The `modules-load.d` file we write.
    pub modules_conf: PathBuf,
    /// The device node whose absence means the module is not loaded.
    pub uinput_node: PathBuf,
    /// The group database we read configured membership from.
    pub group_file: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            udev_rule: PathBuf::from(UDEV_RULE_PATH),
            modules_conf: PathBuf::from(MODULES_CONF_PATH),
            uinput_node: PathBuf::from(UINPUT_PATH),
            group_file: PathBuf::from(GROUP_FILE),
        }
    }
}

/// One thing `setup` will do, in the order DESIGN.md lists them.
///
/// The flags are decided once, while the plan is derived, so that what we print
/// and what we do cannot disagree about the state of the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Write the udev rule which hands `/dev/uinput` to the `input` group.
    UdevRule {
        /// Whether the file is absent (as opposed to present but missing the
        /// rule), which is the difference between "create" and "replace".
        create: bool,
    },
    /// Make sure the `uinput` module is loaded, now and at every boot.
    UinputModule {
        /// Whether the `modules-load.d` file still needs writing; when it is
        /// already in place, only the `modprobe` is left to do.
        write_conf: bool,
    },
    /// Add the user to the `input` group.
    InputGroup,
    /// Reload the udev rules so a newly written one takes effect.
    ReloadUdev,
}

/// A single privileged operation, kept as data so that it can be rendered as a
/// shell command *and* spawned, from one description.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// Write `content` (plus a trailing newline) to `path`, via `tee`.
    Write { path: PathBuf, content: String },
    /// Run a program with arguments.
    Run {
        program: &'static str,
        args: Vec<String>,
    },
}

/// Configures this system, returning the exit code to leave with.
pub async fn run(args: SetupArgs) -> Result<i32, crate::Error> {
    let paths = Paths::default();
    let username = doctor::current_username()?;
    let root = is_root();

    let steps = plan(&paths, &username);
    if steps.is_empty() {
        println!(
            "voice-orders is already configured on this machine — nothing needs changing. Run 'voice-orders doctor' to confirm."
        );
        return Ok(0);
    }

    print!("{}", render_plan(&steps, &username, &paths));

    if args.print {
        print!("{}", render_shell(&steps, &username, &paths, root));
        return Ok(0);
    }

    if !args.yes && !confirm().await? {
        println!("Nothing was changed.");
        return Ok(0);
    }

    for step in &steps {
        apply(step, &username, &paths, root).await?;
    }

    print!("{}", render_epilogue(&steps));
    Ok(0)
}

// ── Deriving the plan ───────────────────────────────────────────────────────

/// Works out which steps this machine still needs.
///
/// Each condition is the inverse of the `doctor` check it answers, so a step
/// only ever appears when the corresponding check would print a `✗`.
fn plan(paths: &Paths, username: &str) -> Vec<Step> {
    let mut steps = Vec::new();

    if !rule_is_present(&paths.udev_rule) {
        steps.push(Step::UdevRule {
            create: !paths.udev_rule.exists(),
        });
    }

    // The conf makes it survive a reboot and the modprobe makes it true now;
    // either one being missing is a reason to do the step.
    let conf = module_conf_is_present(&paths.modules_conf);
    if !doctor::check_uinput_node(&paths.uinput_node).ok || !conf {
        steps.push(Step::UinputModule { write_conf: !conf });
    }

    if !is_in_input_group(&paths.group_file, username) {
        steps.push(Step::InputGroup);
    }

    // udev only reads its rules when it is told to, so a rule we just wrote is
    // inert until we say so — and there is nothing to reload if we wrote none.
    if steps
        .iter()
        .any(|step| matches!(step, Step::UdevRule { .. }))
    {
        steps.push(Step::ReloadUdev);
    }

    debug!(steps = steps.len(), "Derived the setup plan.");
    steps
}

/// Whether the udev rule file already carries our rule.
///
/// A file which exists but says something else still needs writing — that is
/// the case where somebody edited it, or an older version of us wrote it.
fn rule_is_present(path: &Path) -> bool {
    read_or_empty(path)
        .lines()
        .any(|line| line.trim() == UDEV_RULE)
}

/// Whether the `modules-load.d` file already asks for `uinput`.
fn module_conf_is_present(path: &Path) -> bool {
    read_or_empty(path)
        .lines()
        .any(|line| line.trim() == UINPUT_MODULE)
}

/// Whether `/etc/group` lists this user in the `input` group.
///
/// This is deliberately the *configured* membership rather than the effective
/// one: `usermod` is what `setup` can change, and re-running it would not help
/// a session which simply predates the change.
fn is_in_input_group(group_file: &Path, username: &str) -> bool {
    doctor::is_configured(
        doctor::parse_input_group(&read_or_empty(group_file)).as_ref(),
        username,
    )
}

/// Reads a file, treating "we could not read it" as "it has nothing to say".
///
/// An unreadable `/etc/group` or rule file means we cannot prove the system is
/// already configured, and doing a step which was not needed is far cheaper
/// than skipping one which was.
fn read_or_empty(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| {
        debug!("We could not read {}: {e}", path.display());
        String::new()
    })
}

// ── Rendering the plan ──────────────────────────────────────────────────────

/// The plan as it is printed before we ask for confirmation.
fn render_plan(steps: &[Step], username: &str, paths: &Paths) -> String {
    let mut out = String::from("voice-orders setup will make the following changes:\n\n");

    for (index, step) in steps.iter().enumerate() {
        let lines = describe(step, username, paths);
        out.push_str(&format!("  {}. {}\n", index + 1, lines[0]));
        for line in &lines[1..] {
            out.push_str(&format!("     {line}\n"));
        }
    }

    out.push('\n');
    out
}

/// One step in words: what it does, and (for a file) what will be in it.
fn describe(step: &Step, username: &str, paths: &Paths) -> Vec<String> {
    match step {
        Step::UdevRule { create } => vec![
            format!(
                "{} {}",
                if *create { "create" } else { "rewrite" },
                paths.udev_rule.display()
            ),
            UDEV_RULE.to_string(),
        ],
        Step::UinputModule { write_conf: true } => vec![format!(
            "create {} containing '{UINPUT_MODULE}', and load the module now",
            paths.modules_conf.display()
        )],
        Step::UinputModule { write_conf: false } => vec![format!(
            "load the '{UINPUT_MODULE}' module now ({} already asks for it at boot)",
            paths.modules_conf.display()
        )],
        Step::InputGroup => vec![format!(
            "add you to the '{INPUT_GROUP}' group (usermod -aG {INPUT_GROUP} {username})"
        )],
        Step::ReloadUdev => {
            vec![
                "reload the udev rules (udevadm control --reload-rules && udevadm trigger)"
                    .to_string(),
            ]
        }
    }
}

/// The `--print` output: the same steps as shell commands you can paste.
fn render_shell(steps: &[Step], username: &str, paths: &Paths, root: bool) -> String {
    let mut out = String::from("The equivalent shell commands are:\n\n");

    for step in steps {
        for action in actions(step, username, paths) {
            out.push_str("  ");
            out.push_str(&action.shell(root));
            out.push('\n');
        }
    }

    out.push_str("\nNothing was changed. Run 'voice-orders doctor' once you have applied them.\n");
    out
}

/// What to say once the changes have landed.
fn render_epilogue(steps: &[Step]) -> String {
    let mut out = String::from("\nDone.\n");

    if steps.contains(&Step::InputGroup) {
        out.push_str(
            "Your new 'input' group membership only applies to sessions started after it was granted, so log out and back in (or reboot) before running voice-orders.\n",
        );
    }

    out.push_str("Run 'voice-orders doctor' to check the result.\n");
    out
}

// ── Applying the plan ───────────────────────────────────────────────────────

/// The privileged operations one step is made of.
fn actions(step: &Step, username: &str, paths: &Paths) -> Vec<Action> {
    match step {
        Step::UdevRule { .. } => vec![Action::Write {
            path: paths.udev_rule.clone(),
            content: UDEV_RULE.to_string(),
        }],
        Step::UinputModule { write_conf } => {
            let mut actions = Vec::new();
            if *write_conf {
                actions.push(Action::Write {
                    path: paths.modules_conf.clone(),
                    content: UINPUT_MODULE.to_string(),
                });
            }
            actions.push(Action::Run {
                program: "modprobe",
                args: vec![UINPUT_MODULE.to_string()],
            });
            actions
        }
        Step::InputGroup => vec![Action::Run {
            program: "usermod",
            args: vec![
                "-aG".to_string(),
                INPUT_GROUP.to_string(),
                username.to_string(),
            ],
        }],
        Step::ReloadUdev => vec![
            Action::Run {
                program: "udevadm",
                args: vec!["control".to_string(), "--reload-rules".to_string()],
            },
            Action::Run {
                program: "udevadm",
                args: vec!["trigger".to_string()],
            },
        ],
    }
}

impl Action {
    /// The action as a shell command you could paste into a terminal.
    ///
    /// File writes go through `tee` rather than a redirection because the
    /// redirection would be performed by *your* shell, before `sudo` ever runs,
    /// and would therefore be refused — the single most common mistake in
    /// hand-written setup instructions.
    fn shell(&self, root: bool) -> String {
        let sudo = if root { "" } else { "sudo " };

        match self {
            Action::Write { path, content } => format!(
                "echo {} | {sudo}tee {}",
                quote(content),
                quote(&path.display().to_string())
            ),
            Action::Run { program, args } => {
                let mut out = format!("{sudo}{program}");
                for arg in args {
                    out.push(' ');
                    out.push_str(&quote(arg));
                }
                out
            }
        }
    }

    /// What this action is called in a failure message.
    fn what(&self) -> String {
        match self {
            Action::Write { path, .. } => format!("writing {}", path.display()),
            Action::Run { program, .. } => format!("running {program}"),
        }
    }
}

/// Whether a string can be pasted into a shell as-is, or needs quoting.
fn quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c));

    if safe {
        return value.to_string();
    }

    // POSIX single quotes protect everything except a single quote itself,
    // which has to be closed, escaped and reopened.
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Whether we are already root, in which case nothing needs elevating.
fn is_root() -> bool {
    // SAFETY: `geteuid` takes no arguments, cannot fail, and reads only the
    // calling process's own credentials.
    unsafe { libc::geteuid() == 0 }
}

/// Runs every action in a step, failing loudly and by name.
async fn apply(step: &Step, username: &str, paths: &Paths, root: bool) -> Result<(), crate::Error> {
    let description = describe(step, username, paths).remove(0);
    info!("Applying setup step: {description}");

    for action in actions(step, username, paths) {
        execute(&action, root).await.map_err(|e| {
            human_errors::user(
                format!(
                    "The setup step '{description}' failed while {}: {e}",
                    action.what()
                ),
                STEP_FAILURE_ADVICE,
            )
        })?;
    }

    println!("  ✓ {description}");
    Ok(())
}

/// Spawns one action, elevating it unless we are already root.
///
/// Stdio is inherited so that `sudo`'s password prompt behaves normally; a
/// file write is the one exception, since its content has to reach `tee`'s
/// standard input (sudo reads the password from the terminal directly, so
/// piping stdin does not get in its way).
async fn execute(action: &Action, root: bool) -> Result<(), String> {
    let mut command = match action {
        Action::Write { path, .. } => build(root, "tee", &[path.display().to_string()]),
        Action::Run { program, args } => build(root, program, args),
    };

    if let Action::Write { content, .. } = action {
        command
            .stdin(std::process::Stdio::piped())
            // `tee` echoes what it writes, which would just be noise here.
            .stdout(std::process::Stdio::null());

        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "we could not write to the command's standard input".to_string())?;
        stdin
            .write_all(format!("{content}\n").as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        // Closing the pipe is what tells `tee` it has the whole file.
        drop(stdin);

        return status(child.wait().await.map_err(|e| e.to_string())?);
    }

    let child = command.spawn().map_err(|e| e.to_string())?;
    status(
        child
            .wait_with_output()
            .await
            .map_err(|e| e.to_string())?
            .status,
    )
}

/// Builds the command, prefixing `sudo` when we are not root.
fn build(root: bool, program: &str, args: &[String]) -> tokio::process::Command {
    let mut command = if root {
        tokio::process::Command::new(program)
    } else {
        let mut command = tokio::process::Command::new("sudo");
        command.arg(program);
        command
    };

    command.args(args);
    command
}

fn status(status: std::process::ExitStatus) -> Result<(), String> {
    if status.success() {
        return Ok(());
    }

    Err(match status.code() {
        Some(code) => format!("it exited with status {code}"),
        None => "it was killed by a signal".to_string(),
    })
}

// ── The confirmation gate ───────────────────────────────────────────────────

/// Asks whether to go ahead, refusing to guess when nobody is there to answer.
async fn confirm() -> Result<bool, crate::Error> {
    if !stdin_is_a_terminal() {
        return Err(human_errors::user(
            "voice-orders setup makes changes to your system, so it asks you to confirm them first — but its input is not a terminal, so there is nobody to ask.",
            &[
                "Re-run with '--yes' if you have read the plan above and want it applied unattended.",
                "Or run 'voice-orders setup --print' to get the equivalent shell commands and apply them yourself.",
            ],
        ));
    }

    print!("Continue? [y/N] ");
    use std::io::Write;
    let _ = std::io::stdout().flush();

    let mut line = String::new();
    tokio::io::AsyncBufReadExt::read_line(
        &mut tokio::io::BufReader::new(tokio::io::stdin()),
        &mut line,
    )
    .await
    .map_err(|e| {
        human_errors::wrap_system(
            e,
            "We could not read your answer from the terminal.",
            &["Re-run with '--yes' to apply the plan without being asked."],
        )
    })?;

    Ok(confirmed(&line))
}

/// Whether an answer to `[y/N]` means yes. Anything else — including the empty
/// line you get from pressing return — means no, which is what the capital `N`
/// in the prompt promises.
fn confirmed(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

fn stdin_is_a_terminal() -> bool {
    // SAFETY: `isatty` only inspects the descriptor it is given, and 0 is
    // always a valid descriptor number to ask about.
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// A machine's `/etc` and `/dev`, in a temporary directory.
    ///
    /// Nothing in this module's tests may touch the real ones, so every test
    /// builds its world here and hands the paths to the code under test.
    struct Machine {
        dir: tempfile::TempDir,
        paths: Paths,
    }

    impl Machine {
        /// A machine with nothing configured: no rule, no conf, no uinput node
        /// and an `/etc/group` whose `input` group is empty.
        fn bare() -> Self {
            let dir = tempfile::tempdir().expect("a temporary directory");
            let paths = Paths {
                udev_rule: dir.path().join("60-voice-orders-uinput.rules"),
                modules_conf: dir.path().join("voice-orders.conf"),
                uinput_node: dir.path().join("uinput"),
                group_file: dir.path().join("group"),
            };

            let machine = Self { dir, paths };
            machine.write("group", "root:x:0:\ninput:x:992:\n");
            machine
        }

        fn write(&self, name: &str, content: &str) -> &Self {
            std::fs::write(self.dir.path().join(name), content)
                .expect("the file should be written");
            self
        }

        /// The udev rule already in place.
        fn with_rule(&self) -> &Self {
            self.write(
                "60-voice-orders-uinput.rules",
                &format!("# written by voice-orders\n{UDEV_RULE}\n"),
            )
        }

        /// The modules-load.d file already in place.
        fn with_module_conf(&self) -> &Self {
            self.write("voice-orders.conf", "uinput\n")
        }

        /// The uinput device node present, i.e. the module loaded.
        fn with_uinput_node(&self) -> &Self {
            self.write("uinput", "")
        }

        /// `alice` already a member of the `input` group.
        fn with_group_membership(&self) -> &Self {
            self.write("group", "root:x:0:\ninput:x:992:alice\n")
        }

        /// Everything configured — the state `setup` must leave alone.
        fn fully_configured(&self) -> &Self {
            self.with_rule()
                .with_module_conf()
                .with_uinput_node()
                .with_group_membership()
        }

        fn plan(&self) -> Vec<Step> {
            plan(&self.paths, "alice")
        }
    }

    #[test]
    fn test_a_bare_machine_needs_all_four_steps() {
        let machine = Machine::bare();

        assert_eq!(
            machine.plan(),
            vec![
                Step::UdevRule { create: true },
                Step::UinputModule { write_conf: true },
                Step::InputGroup,
                Step::ReloadUdev,
            ],
            "the four steps must be planned in the order DESIGN.md lists them"
        );
    }

    #[test]
    fn test_a_configured_machine_needs_nothing() {
        let machine = Machine::bare();
        machine.fully_configured();

        assert!(
            machine.plan().is_empty(),
            "nothing which is already configured may be touched"
        );
    }

    #[test]
    fn test_an_existing_rule_file_without_the_rule_is_rewritten() {
        let machine = Machine::bare();
        machine
            .fully_configured()
            .write("60-voice-orders-uinput.rules", "# somebody emptied this\n");

        assert_eq!(
            machine.plan(),
            vec![Step::UdevRule { create: false }, Step::ReloadUdev],
            "a file which exists but lacks the rule still needs writing, and a rewritten rule needs reloading"
        );
    }

    #[test]
    fn test_a_missing_uinput_node_reloads_the_module_but_keeps_the_conf() {
        let machine = Machine::bare();
        machine.fully_configured();
        std::fs::remove_file(&machine.paths.uinput_node).expect("the node should be removed");

        assert_eq!(
            machine.plan(),
            vec![Step::UinputModule { write_conf: false }],
            "the conf is already right; only the module still needs loading"
        );
        assert_eq!(
            actions(&machine.plan()[0], "alice", &machine.paths),
            vec![Action::Run {
                program: "modprobe",
                args: vec!["uinput".to_string()],
            }],
            "a conf which is already in place must not be rewritten"
        );
    }

    #[test]
    fn test_a_missing_conf_is_written_even_when_the_module_is_loaded() {
        let machine = Machine::bare();
        machine.fully_configured();
        std::fs::remove_file(&machine.paths.modules_conf).expect("the conf should be removed");

        assert_eq!(
            machine.plan(),
            vec![Step::UinputModule { write_conf: true }],
            "the module is loaded now, but nothing would load it at the next boot"
        );
    }

    #[test]
    fn test_a_missing_group_membership_is_the_only_step_left() {
        let machine = Machine::bare();
        machine
            .fully_configured()
            .write("group", "root:x:0:\ninput:x:992:bob\n");

        assert_eq!(machine.plan(), vec![Step::InputGroup]);
    }

    #[test]
    fn test_no_udev_rule_is_written_means_no_reload() {
        let machine = Machine::bare();
        machine.with_rule();

        let steps = machine.plan();
        assert!(
            !steps.contains(&Step::ReloadUdev),
            "there is no new rule to reload: {steps:?}"
        );
    }

    #[test]
    fn test_an_unreadable_group_file_plans_the_group_step() {
        // We cannot prove membership, and adding somebody who is already a
        // member is harmless, so the safe assumption is that the step is needed.
        let machine = Machine::bare();
        machine.fully_configured();
        std::fs::remove_file(&machine.paths.group_file).expect("the group file should be removed");

        assert_eq!(machine.plan(), vec![Step::InputGroup]);
    }

    #[test]
    fn test_the_udev_rule_is_the_one_the_documentation_promises() {
        assert_eq!(UDEV_RULE, r#"KERNEL=="uinput", GROUP="input", MODE="0660""#);

        let machine = Machine::bare();
        assert_eq!(
            actions(&Step::UdevRule { create: true }, "alice", &machine.paths),
            vec![Action::Write {
                path: machine.paths.udev_rule.clone(),
                content: r#"KERNEL=="uinput", GROUP="input", MODE="0660""#.to_string(),
            }]
        );
    }

    #[test]
    fn test_the_module_conf_contains_only_the_module_name() {
        let machine = Machine::bare();
        let actions = actions(
            &Step::UinputModule { write_conf: true },
            "alice",
            &machine.paths,
        );

        assert_eq!(
            actions[0],
            Action::Write {
                path: machine.paths.modules_conf.clone(),
                content: "uinput".to_string(),
            }
        );
        assert_eq!(
            actions[1],
            Action::Run {
                program: "modprobe",
                args: vec!["uinput".to_string()],
            },
            "the module must also be loaded now, not only at the next boot"
        );
    }

    #[test]
    fn test_the_plan_reads_like_the_permissions_guide() {
        let paths = Paths::default();
        let steps = vec![
            Step::UdevRule { create: true },
            Step::UinputModule { write_conf: true },
            Step::InputGroup,
            Step::ReloadUdev,
        ];

        let rendered = render_plan(&steps, "you", &paths);

        assert!(
            rendered.starts_with("voice-orders setup will make the following changes:\n\n"),
            "unexpected plan:\n{rendered}"
        );
        for expected in [
            "  1. create /etc/udev/rules.d/60-voice-orders-uinput.rules\n",
            "     KERNEL==\"uinput\", GROUP=\"input\", MODE=\"0660\"\n",
            "  2. create /etc/modules-load.d/voice-orders.conf containing 'uinput', and load the module now\n",
            "  3. add you to the 'input' group (usermod -aG input you)\n",
            "  4. reload the udev rules (udevadm control --reload-rules && udevadm trigger)\n",
        ] {
            assert!(
                rendered.contains(expected),
                "the plan should contain {expected:?}, got:\n{rendered}"
            );
        }
    }

    #[test]
    fn test_a_partial_plan_is_numbered_from_one() {
        let rendered = render_plan(&[Step::InputGroup], "alice", &Paths::default());

        assert!(
            rendered.contains("  1. add you to the 'input' group (usermod -aG input alice)\n"),
            "only the missing pieces are listed, and they are numbered as printed:\n{rendered}"
        );
        assert!(
            !rendered.contains("udev"),
            "nothing which is already configured may be mentioned:\n{rendered}"
        );
    }

    #[test]
    fn test_the_shell_equivalents_are_paste_able() {
        let steps = vec![
            Step::UdevRule { create: true },
            Step::UinputModule { write_conf: true },
            Step::InputGroup,
            Step::ReloadUdev,
        ];

        let rendered = render_shell(&steps, "alice", &Paths::default(), false);

        for expected in [
            r#"  echo 'KERNEL=="uinput", GROUP="input", MODE="0660"' | sudo tee /etc/udev/rules.d/60-voice-orders-uinput.rules"#,
            "  echo uinput | sudo tee /etc/modules-load.d/voice-orders.conf",
            "  sudo modprobe uinput",
            "  sudo usermod -aG input alice",
            "  sudo udevadm control --reload-rules",
            "  sudo udevadm trigger",
        ] {
            assert!(
                rendered.contains(expected),
                "the shell output should contain {expected:?}, got:\n{rendered}"
            );
        }

        assert!(
            rendered.contains("Nothing was changed."),
            "--print must promise it changed nothing:\n{rendered}"
        );
    }

    #[test]
    fn test_the_shell_equivalents_drop_sudo_for_root() {
        let rendered = render_shell(
            &[Step::UdevRule { create: true }, Step::ReloadUdev],
            "root",
            &Paths::default(),
            true,
        );

        assert!(
            !rendered.contains("sudo"),
            "root has nothing to elevate to:\n{rendered}"
        );
        assert!(
            rendered.contains(
                r#"echo 'KERNEL=="uinput", GROUP="input", MODE="0660"' | tee /etc/udev/rules.d/60-voice-orders-uinput.rules"#
            ),
            "unexpected output:\n{rendered}"
        );
    }

    #[test]
    fn test_a_file_write_never_redirects_in_the_calling_shell() {
        let action = Action::Write {
            path: PathBuf::from("/etc/udev/rules.d/60-voice-orders-uinput.rules"),
            content: UDEV_RULE.to_string(),
        };

        let rendered = action.shell(false);
        assert!(
            !rendered.contains('>'),
            "a '>' would be performed by your shell, before sudo runs, and would be denied: {rendered}"
        );
        assert!(rendered.contains("| sudo tee "), "{rendered}");
    }

    #[rstest]
    // Nothing which needs a shell's attention is quoted...
    #[case("uinput", "uinput")]
    #[case("-aG", "-aG")]
    #[case(
        "/etc/modules-load.d/voice-orders.conf",
        "/etc/modules-load.d/voice-orders.conf"
    )]
    // ...and everything which does, is.
    #[case(
        r#"KERNEL=="uinput", GROUP="input""#,
        r#"'KERNEL=="uinput", GROUP="input"'"#
    )]
    #[case("two words", "'two words'")]
    #[case("", "''")]
    #[case("rm -rf /; echo", "'rm -rf /; echo'")]
    // A single quote has to be closed, escaped, and reopened.
    #[case("it's", r#"'it'\''s'"#)]
    fn test_shell_quoting(#[case] value: &str, #[case] expected: &str) {
        assert_eq!(quote(value), expected);
    }

    #[rstest]
    #[case::yes("y\n", true)]
    #[case::uppercase("Y\n", true)]
    #[case::spelled_out("yes\n", true)]
    #[case::mixed_case("Yes\n", true)]
    #[case::padded("  y  \n", true)]
    // The capital N in "[y/N]" promises that everything else is a no.
    #[case::empty("\n", false)]
    #[case::no("n\n", false)]
    #[case::eof("", false)]
    #[case::something_else("maybe\n", false)]
    #[case::almost("yep\n", false)]
    fn test_only_an_explicit_yes_proceeds(#[case] answer: &str, #[case] expected: bool) {
        assert_eq!(confirmed(answer), expected);
    }

    #[test]
    fn test_the_epilogue_asks_for_a_re_login_only_when_the_group_changed() {
        let with_group = render_epilogue(&[Step::InputGroup]);
        assert!(
            with_group.contains("log out and back in"),
            "unexpected epilogue:\n{with_group}"
        );
        assert!(with_group.contains("voice-orders doctor"));

        let without_group = render_epilogue(&[Step::ReloadUdev]);
        assert!(
            !without_group.contains("log out and back in"),
            "no membership changed, so there is nothing to re-login for:\n{without_group}"
        );
        assert!(
            without_group.contains("voice-orders doctor"),
            "the verification step is always worth suggesting:\n{without_group}"
        );
    }

    #[test]
    fn test_a_failed_step_is_named_in_the_error() {
        // The message is built where the failure is caught; this is the shape
        // of it, without ever running anything privileged.
        let paths = Paths::default();
        let description = describe(&Step::InputGroup, "alice", &paths).remove(0);
        let error = human_errors::user(
            format!(
                "The setup step '{description}' failed while {}: it exited with status 1",
                Action::Run {
                    program: "usermod",
                    args: Vec::new()
                }
                .what()
            ),
            STEP_FAILURE_ADVICE,
        );

        assert!(error.is(human_errors::Kind::User));
        assert!(
            error.description().contains("add you to the 'input' group"),
            "the error should name the step which failed: {}",
            error.description()
        );
    }

    /// The one part of the execution path a test may safely exercise: a file
    /// write, unelevated, into a temporary directory.
    ///
    /// It is worth doing because the piping is the subtle half — `tee` gets the
    /// content on its standard input and only finishes when that pipe closes —
    /// and nothing about `sudo` or `/etc` is involved in getting it right.
    #[tokio::test]
    async fn test_a_file_write_reaches_the_file() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("60-voice-orders-uinput.rules");

        execute(
            &Action::Write {
                path: path.clone(),
                content: UDEV_RULE.to_string(),
            },
            // Already "root": nothing is elevated, so this runs plain `tee`.
            true,
        )
        .await
        .expect("writing into a temporary directory should work");

        assert_eq!(
            std::fs::read_to_string(&path).expect("the file should exist"),
            format!("{UDEV_RULE}\n"),
            "the rule should land exactly once, with a trailing newline"
        );
    }

    #[test]
    fn test_the_real_paths_are_the_documented_ones() {
        let paths = Paths::default();

        assert_eq!(
            paths.udev_rule,
            PathBuf::from("/etc/udev/rules.d/60-voice-orders-uinput.rules")
        );
        assert_eq!(
            paths.modules_conf,
            PathBuf::from("/etc/modules-load.d/voice-orders.conf")
        );
        assert_eq!(paths.uinput_node, PathBuf::from("/dev/uinput"));
        assert_eq!(paths.group_file, PathBuf::from("/etc/group"));
    }
}
