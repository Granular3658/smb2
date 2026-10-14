//! Working out which username and password to connect with.

use std::io::IsTerminal;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Credentials for one SMB connection. `Debug` shows the password as `***`.
#[derive(Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &format_args!("***"))
            .finish()
    }
}

/// The credential-related flags, shared by every command. `Debug` shows the
/// password as `***`.
#[derive(Clone, clap::Args)]
pub struct AuthArgs {
    /// Username to log in with.
    #[arg(short = 'u', long, env = "SMB2_USER", global = true)]
    pub user: Option<String>,

    /// Password. Prefer `--password-command` so it stays out of your shell history.
    #[arg(long, env = "SMB2_PASS", global = true, hide_env_values = true)]
    pub password: Option<String>,

    /// Shell command that prints the password on stdout, for example
    /// `--password-command 'secret RASPI_SMB_PASSWORD'`.
    #[arg(long, env = "SMB2_PASS_COMMAND", global = true)]
    pub password_command: Option<String>,

    /// Connect as guest (no username or password).
    #[arg(long, global = true, conflicts_with_all = ["user", "password", "password_command"])]
    pub guest: bool,
}

impl std::fmt::Debug for AuthArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthArgs")
            .field("user", &self.user)
            .field(
                "password",
                &self.password.as_ref().map(|_| format_args!("***")),
            )
            .field("password_command", &self.password_command)
            .field("guest", &self.guest)
            .finish()
    }
}

impl AuthArgs {
    /// Resolves credentials, prompting on a terminal when nothing else supplied
    /// a password.
    pub fn resolve(&self) -> Result<Credentials> {
        if self.guest {
            return Ok(Credentials {
                username: String::new(),
                password: String::new(),
            });
        }

        let username = self
            .user
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no username: pass --user or set SMB2_USER"))?;

        if let Some(command) = &self.password_command {
            let output = Command::new("sh")
                .arg("-c")
                .arg(command)
                .output()
                .with_context(|| format!("running --password-command {command:?}"))?;
            if !output.status.success() {
                bail!(
                    "--password-command {command:?} failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            let password = String::from_utf8(output.stdout)
                .context("--password-command printed invalid UTF-8")?
                .trim_end_matches(['\n', '\r'])
                .to_string();
            return Ok(Credentials { username, password });
        }

        if let Some(password) = &self.password {
            return Ok(Credentials {
                username,
                password: password.clone(),
            });
        }

        if std::io::stdin().is_terminal() {
            let password = rpassword::prompt_password(format!("Password for {username}: "))
                .context("reading the password")?;
            return Ok(Credentials { username, password });
        }

        bail!("no password: pass --password-command, set SMB2_PASS, or run on a terminal")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_hides_the_password() {
        let creds = Credentials {
            username: "david".to_string(),
            password: "hunter2-secret".to_string(),
        };
        let args = AuthArgs {
            user: Some("david".to_string()),
            password: Some("hunter2-secret".to_string()),
            password_command: None,
            guest: false,
        };
        for printed in [format!("{creds:?}"), format!("{args:?}")] {
            assert!(!printed.contains("hunter2"), "{printed}");
            assert!(printed.contains("***"), "{printed}");
            assert!(printed.contains("david"), "{printed}");
        }
    }
}
