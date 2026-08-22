//! Shared `-p<password>` argument.

use clap::Args;

/// Common `-p<password>` argument shared by every command.
#[derive(Args)]
pub struct PasswordArgs {
    /// Archive password (empty with bare `-p`)
    #[arg(
        short = 'p',
        long,
        global = true,
        value_name = "PASSWORD",
        num_args = 0..=1,
        default_missing_value = ""
    )]
    pub password: Option<String>,
}
