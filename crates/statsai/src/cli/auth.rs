use anyhow::Result;
use statsai::auth;

use super::args::{AuthCommand, AuthSubcommand};

pub(crate) fn auth(command: AuthCommand) -> Result<()> {
    match command.command {
        AuthSubcommand::Login {
            no_open,
            headless,
            device_name,
        } => auth::login(no_open, headless, device_name),
        AuthSubcommand::Status => auth::status(),
        AuthSubcommand::Logout => auth::logout(),
    }
}
