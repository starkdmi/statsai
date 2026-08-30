use anyhow::Result;
use clap::Parser;
use statsai::{default_device_id, default_store_path, snapshot};
use statsai_store::Store;

use super::*;

pub(crate) fn run() -> Result<()> {
    let cli = Cli::parse();
    let store_path = cli.store.unwrap_or_else(default_store_path);
    let device_id = cli.device_id.unwrap_or_else(default_device_id);

    match cli.command {
        Command::Schema(command) => schema(command),
        Command::Store(command) => store_admin(command, &store_path),
        Command::Doctor => doctor(&store_path),
        Command::Auth(command) => auth(command),
        Command::Service(command) => service(command),
        Command::Snapshot(command) => snapshot::run(command, &store_path, &device_id),
        command => {
            let store = if command_reprices_persisted_usage(&command) {
                statsai::open_operational_store(&store_path)?
            } else {
                Store::open(&store_path)?
            };
            match command {
                Command::Scan(command) => scan(command, &store, &device_id),
                Command::Report(command) => report(command, &store),
                Command::Source(command) => source(command, &store, &device_id),
                Command::Account(command) => account(command, &store),
                Command::Subscription(command) => subscription(command, &store),
                Command::Import(command) => import(command, &store, &device_id),
                Command::Export(command) => export(command, &store),
                Command::Task(command) => task(command, &store),
                Command::Conversation(command) => conversation(command, &store, &device_id),
                Command::Quota(command) => quota(command, &store, &device_id),
                Command::Privacy(command) => {
                    statsai::privacy_cli::run(command, &store, &store_path)
                }
                Command::Sync(command) => sync(command, &store, &device_id),
                Command::Daemon(command) => daemon(command, store, &device_id),
                Command::Status => status(&store),
                Command::Schema(_)
                | Command::Store(_)
                | Command::Doctor
                | Command::Auth(_)
                | Command::Service(_)
                | Command::Snapshot(_) => {
                    unreachable!("handled before store open")
                }
            }
        }
    }
}

pub(crate) fn command_reprices_persisted_usage(command: &Command) -> bool {
    matches!(
        command,
        Command::Scan(_)
            | Command::Report(_)
            | Command::Import(_)
            | Command::Export(_)
            | Command::Task(_)
            | Command::Sync(_)
            | Command::Daemon(_)
    )
}
