use super::*;

#[test]
fn location_path_hash_matches_path_hash_for_an_existing_folder() {
    let dir = tempfile::tempdir().expect("tempdir");
    let folder = dir.path().join("checkout");
    std::fs::create_dir_all(&folder).expect("folder");

    assert_eq!(location_path_hash(&folder), path_hash(&folder));
}

#[test]
fn location_path_hash_survives_deleting_the_folder() {
    let dir = tempfile::tempdir().expect("tempdir");
    let folder = dir.path().join("workspace").join("checkout");
    std::fs::create_dir_all(&folder).expect("folder");
    let while_present = location_path_hash(&folder);

    std::fs::remove_dir_all(dir.path().join("workspace")).expect("delete");

    assert_eq!(location_path_hash(&folder), while_present);
}

#[cfg(unix)]
#[test]
fn location_path_hash_resolves_a_symlinked_ancestor_of_a_deleted_folder() {
    let dir = tempfile::tempdir().expect("tempdir");
    let real = dir.path().join("real");
    std::fs::create_dir_all(real.join("checkout")).expect("folder");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");
    let folder = link.join("checkout");
    let while_present = location_path_hash(&folder);
    assert_eq!(while_present, path_hash(&real.join("checkout")));

    std::fs::remove_dir_all(real.join("checkout")).expect("delete");

    assert_ne!(
        path_hash(&folder),
        while_present,
        "path_hash falls back to the symlinked spelling"
    );
    assert_eq!(location_path_hash(&folder), while_present);
}
