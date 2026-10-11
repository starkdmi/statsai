use anyhow::Result;
use statsai_store::Store;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::args::DaemonCommand;

pub(crate) fn daemon(
    command: DaemonCommand,
    store: Store,
    device_id: &str,
    store_path: &Path,
) -> Result<()> {
    let store = Arc::new(Mutex::new(store));
    let auth_token = statsai::default_daemon_auth_token()?;
    if command.watch {
        // The same lock `statsai scan` takes for this store, held per pass.
        let scan_lock = statsai::scan_lock_path(store_path);
        statsai_daemon::watch_and_serve(
            &command.api,
            store,
            device_id,
            &auth_token,
            Some(&scan_lock),
        )
    } else {
        statsai_daemon::run(&command.api, store, &auth_token)
    }
}
