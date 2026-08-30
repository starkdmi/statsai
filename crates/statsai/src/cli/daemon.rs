use anyhow::Result;
use statsai_store::Store;
use std::sync::{Arc, Mutex};

use super::args::DaemonCommand;

pub(crate) fn daemon(command: DaemonCommand, store: Store, device_id: &str) -> Result<()> {
    let store = Arc::new(Mutex::new(store));
    let auth_token = statsai::default_daemon_auth_token()?;
    if command.watch {
        statsai_daemon::watch_and_serve(&command.api, store, device_id, &auth_token)
    } else {
        statsai_daemon::run(&command.api, store, &auth_token)
    }
}
