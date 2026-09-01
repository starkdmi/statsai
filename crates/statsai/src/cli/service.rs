use anyhow::Result;
use statsai::service;

use super::args::{ServiceCommand, ServiceSubcommand};

pub(crate) fn service(command: ServiceCommand) -> Result<()> {
    use service::ServiceAction;

    match command.command {
        ServiceSubcommand::Install => service::service(ServiceAction::Install),
        ServiceSubcommand::Uninstall => service::service(ServiceAction::Uninstall),
        ServiceSubcommand::Status => service::service(ServiceAction::Status),
    }
}
