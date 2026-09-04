//! Shared `-p<password>` argument and prompt-safety validation.

use clap::Args;

/// Common `-p<password>` argument shared by every command.
#[derive(Args)]
pub struct PasswordArgs {
    /// Archive password (`-p-` explicitly disables password use)
    #[arg(
        short = 'p',
        long,
        global = true,
        value_name = "PASSWORD",
        num_args = 1
    )]
    pub password: Option<String>,
}

/// Reject password switches that are syntactically missing a value. Bare
/// short `-p` is normalized to the private `--password-prompt` marker so it
/// cannot be confused with `--password secret`. `-p-` normalizes to an empty
/// explicit value and is intentionally accepted.
pub fn reject_bare_password(args: &[String]) -> Result<(), String> {
    let missing_value = args.iter().enumerate().any(|(index, arg)| {
        arg == "--password-prompt"
            || (arg == "--password" && args.get(index + 1).is_none_or(|next| next.starts_with('-')))
    });
    if missing_value {
        Err("bare -p requires a secure no-echo password prompt, which is not supported; use -p<PASSWORD>, --password PASSWORD, or -p- to disable password use".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::reject_bare_password;

    #[test]
    fn bare_password_is_distinct_from_explicit_values_and_disable() {
        assert!(reject_bare_password(&["--password-prompt".into()]).is_err());
        assert!(reject_bare_password(&["--password".into()]).is_err());
        assert!(reject_bare_password(&["--password".into(), "secret".into()]).is_ok());
        assert!(reject_bare_password(&["--password=".into()]).is_ok());
        assert!(reject_bare_password(&["--password=secret".into()]).is_ok());
    }
}
