use super::support::*;
use super::*;

#[test]
fn status_sync_does_not_persist_sync_preferences() {
    let store = Store::in_memory().expect("store");

    sync(
        SyncCommand {
            status: true,
            include_tasks: true,
            ..test_sync_command("file")
        },
        &store,
        "device",
    )
    .expect("sync status");

    assert_eq!(
        store.sync_preferences().expect("sync preferences"),
        SyncPreferences::default()
    );
}
