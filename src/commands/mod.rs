use clap::Subcommand;

mod devices;
mod doctor;
mod new;
mod run;
mod setup;
mod test;
mod ui;
mod validate;

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Scaffold a new profile YAML with commented examples.
    New(new::NewArgs),
    /// Check a profile: grammar, key names, and every word against the Vosk model's vocabulary.
    Validate(validate::ValidateArgs),
    /// Listen and report what would happen, without typing anything or needing /dev/uinput.
    Test(test::TestArgs),
    /// Load a profile and start listening; optionally wrap a child process.
    Run(run::RunArgs),
    /// Configure this system: the udev rule, the uinput module and the 'input' group.
    Setup(setup::SetupArgs),
    /// Diagnose this system: /dev/uinput, /dev/input, the microphone and the model.
    Doctor(doctor::DoctorArgs),
    /// List this machine's microphones and input devices, as audio.device and hotkey.device see them.
    Devices(devices::DevicesArgs),
}

/// Dispatches the parsed CLI arguments to the matching subcommand, returning
/// the process exit code (`run` propagates its child's exit code).
pub async fn dispatch(args: crate::Args) -> Result<i32, crate::Error> {
    match args.command {
        Command::New(args) => new::run(args).await.map(|_| 0),
        Command::Validate(args) => validate::run(args).await,
        Command::Test(args) => test::run(args).await,
        Command::Run(args) => run::run(args).await,
        Command::Setup(args) => setup::run(args).await,
        Command::Doctor(args) => doctor::run(args).await,
        Command::Devices(args) => devices::run(args).await,
    }
}
