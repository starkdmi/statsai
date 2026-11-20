//! `local_metadata` values: [`Store::compare_and_set_metadata_value`].

use super::*;

const KEY: &str = "test.compare_and_set";

fn value(store: &Store) -> Option<String> {
    store.metadata_value(KEY).expect("metadata value")
}

#[test]
fn compare_and_set_sets_an_absent_value_only_when_absence_is_expected() {
    let store = Store::in_memory().expect("store");

    assert!(!store
        .compare_and_set_metadata_value(KEY, Some("first"), "second")
        .expect("compare and set"));
    assert_eq!(value(&store), None, "a refused swap writes nothing");

    assert!(store
        .compare_and_set_metadata_value(KEY, None, "first")
        .expect("compare and set"));
    assert_eq!(value(&store).as_deref(), Some("first"));
}

#[test]
fn compare_and_set_replaces_a_matching_value_and_refuses_any_other() {
    let store = Store::in_memory().expect("store");
    store.set_metadata_value(KEY, "first").expect("set");

    assert!(!store
        .compare_and_set_metadata_value(KEY, None, "absent")
        .expect("compare and set"));
    assert!(!store
        .compare_and_set_metadata_value(KEY, Some("other"), "mismatch")
        .expect("compare and set"));
    assert_eq!(value(&store).as_deref(), Some("first"));

    assert!(store
        .compare_and_set_metadata_value(KEY, Some("first"), "second")
        .expect("compare and set"));
    assert_eq!(value(&store).as_deref(), Some("second"));
}

#[test]
fn of_two_connections_that_read_the_same_value_only_one_replaces_it() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("statsai.sqlite");
    let first = Store::open(&path).expect("first connection");
    let second = Store::open(&path).expect("second connection");
    first.set_metadata_value(KEY, "read").expect("set");

    let read_by_first = first.metadata_value(KEY).expect("first read");
    let read_by_second = second.metadata_value(KEY).expect("second read");
    assert_eq!(read_by_first, read_by_second);

    assert!(first
        .compare_and_set_metadata_value(KEY, read_by_first.as_deref(), "first")
        .expect("first swap"));
    assert!(!second
        .compare_and_set_metadata_value(KEY, read_by_second.as_deref(), "second")
        .expect("second swap"));
    assert_eq!(value(&second).as_deref(), Some("first"));
}

#[test]
fn compare_and_set_joins_a_transaction_the_caller_holds() {
    let store = Store::in_memory().expect("store");

    let swapped = store
        .apply_scan_update(|store| -> Result<()> {
            assert!(store.compare_and_set_metadata_value(KEY, None, "inside")?);
            anyhow::bail!("roll the caller's transaction back")
        })
        .expect_err("the caller's transaction fails");
    assert!(swapped.to_string().contains("roll"), "{swapped:#}");
    assert_eq!(value(&store), None, "the swap rolled back with the caller");
}
