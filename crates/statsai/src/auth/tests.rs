use super::logout::logout_backend;
use super::session::{with_device_id_retry, DeviceSessionRequestError};
use super::*;
use std::cell::Cell;

#[test]
fn credential_transport_allows_https_and_explicit_loopback_http_only() {
    for url in [
        "https://api.example.com",
        "http://127.0.0.1:8787",
        "http://127.42.0.1:8787",
        "http://[::1]:8787",
    ] {
        validate_credential_transport_url(url, "test endpoint").expect("secure transport");
    }

    for url in [
        "http://api.example.com",
        "http://localhost:8787",
        "http://127.0.0.1.attacker.example",
        "http://127.0.0.1@attacker.example",
        "ftp://127.0.0.1",
    ] {
        let error =
            validate_credential_transport_url(url, "test endpoint").expect_err("unsafe transport");
        assert!(error.to_string().contains("must use HTTPS"));
    }
}

#[test]
fn stored_session_requires_cloudflare_refresh_token() {
    assert!(!has_cloudflare_session(&AuthCredentials {
        backend: Some("legacy".to_string()),
        api_base_url: None,
        cloudflare_refresh_token: None,
        cloudflare_refresh_expires_at_secs: 0,
        cloudflare_access_token: None,
        cloudflare_access_expires_at_secs: 0,
        device_id: None,
    }));
    assert!(has_cloudflare_session(&AuthCredentials {
        backend: Some("cloudflare".to_string()),
        api_base_url: Some("http://127.0.0.1:8787".to_string()),
        cloudflare_refresh_token: Some("refresh-token".to_string()),
        cloudflare_refresh_expires_at_secs: 0,
        cloudflare_access_token: None,
        cloudflare_access_expires_at_secs: 0,
        device_id: Some("device-1".to_string()),
    }));
}

#[test]
fn auth_path_is_scoped_by_api_url() {
    let dir = tempfile::tempdir().expect("tempdir");
    let local = auth_path_for_api_base_url(dir.path(), "http://127.0.0.1:8787");
    let hosted = auth_path_for_api_base_url(dir.path(), "https://api.example.com");

    assert_ne!(local, hosted);
    assert_eq!(
        local
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::len),
        Some("auth-.json".len() + 64)
    );
    assert_eq!(
        hosted
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::len),
        Some("auth-.json".len() + 64)
    );
}

#[test]
fn missing_auth_metadata_does_not_create_a_refresh_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let auth_base = dir.path().join("missing-auth-directory");
    let api_base_url = "https://api.example.com";

    let token = get_or_refresh_token_from_base(&auth_base, api_base_url)
        .expect("missing credentials are optional");

    assert_eq!(token, None);
    assert!(!auth_base.exists());
    assert!(!auth_path_for_api_base_url(&auth_base, api_base_url)
        .with_extension("lock")
        .exists());
}

#[cfg(unix)]
#[test]
fn read_only_auth_directory_without_metadata_does_not_require_a_refresh_lock() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let auth_base = dir.path().join("read-only-auth-directory");
    std::fs::create_dir(&auth_base).expect("auth directory");
    std::fs::set_permissions(&auth_base, std::fs::Permissions::from_mode(0o500))
        .expect("make auth directory read-only");
    let api_base_url = "https://api.example.com";

    let token = get_or_refresh_token_from_base(&auth_base, api_base_url);

    std::fs::set_permissions(&auth_base, std::fs::Permissions::from_mode(0o700))
        .expect("restore auth directory permissions");
    assert_eq!(token.expect("missing credentials are optional"), None);
    assert!(!auth_path_for_api_base_url(&auth_base, api_base_url)
        .with_extension("lock")
        .exists());
}

#[test]
fn logout_removes_corrupt_scoped_metadata_and_runs_keyring_cleanup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let api_base_url = "https://api.example.com";
    let path = auth_path_for_api_base_url(dir.path(), api_base_url);
    std::fs::write(&path, "{not-json").expect("corrupt auth metadata");
    let pending_path = pending_refresh_rotation_path(&path);
    std::fs::write(&pending_path, "pending").expect("pending rotation");
    let keyring_calls = Cell::new(0);

    let removed = logout_backend(dir.path(), api_base_url, |requested_api_base_url| {
        assert_eq!(requested_api_base_url, api_base_url);
        keyring_calls.set(keyring_calls.get() + 1);
        Ok(())
    })
    .expect("logout");

    assert!(removed);
    assert_eq!(keyring_calls.get(), 1);
    assert!(!path.exists());
    assert!(!pending_path.exists());
}

#[test]
fn logout_removes_metadata_even_when_keyring_cleanup_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let api_base_url = "https://api.example.com";
    let path = auth_path_for_api_base_url(dir.path(), api_base_url);
    std::fs::write(&path, "{not-json").expect("corrupt auth metadata");

    let error = logout_backend(dir.path(), api_base_url, |_| {
        bail!("forced keyring deletion failure")
    })
    .expect_err("keyring failure");

    assert!(error.to_string().contains("OS keyring"));
    assert!(!path.exists());
}

#[test]
fn logout_runs_keyring_cleanup_when_legacy_metadata_is_corrupt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let api_base_url = "https://api.example.com";
    let legacy_path = legacy_auth_path(dir.path());
    std::fs::write(&legacy_path, "{not-json").expect("corrupt legacy auth metadata");
    let keyring_calls = Cell::new(0);

    let error = logout_backend(dir.path(), api_base_url, |_| {
        keyring_calls.set(keyring_calls.get() + 1);
        Ok(())
    })
    .expect_err("legacy metadata parse failure");

    assert!(error.to_string().contains("legacy auth metadata"));
    assert_eq!(keyring_calls.get(), 1);
    assert!(legacy_path.exists());
}

#[test]
fn logout_preserves_legacy_metadata_for_another_backend() {
    let dir = tempfile::tempdir().expect("tempdir");
    let requested_backend = "https://api.statsai.dev";
    let other_backend = "https://api-statsai.dev";
    let legacy_path = legacy_scoped_auth_path(dir.path(), other_backend);
    let credentials = AuthCredentials {
        backend: Some("cloudflare".to_string()),
        api_base_url: Some(other_backend.to_string()),
        cloudflare_refresh_token: None,
        cloudflare_refresh_expires_at_secs: 0,
        cloudflare_access_token: None,
        cloudflare_access_expires_at_secs: 0,
        device_id: Some("other-device".to_string()),
    };
    write_credentials(&legacy_path, &credentials).expect("legacy metadata");

    let removed = logout_backend(dir.path(), requested_backend, |_| Ok(())).expect("logout");

    assert!(!removed);
    assert!(legacy_path.exists());
}

#[test]
fn atomic_auth_write_preserves_destination_on_serialization_failure() {
    struct FailingSerialize;

    impl Serialize for FailingSerialize {
        fn serialize<S>(&self, _serializer: S) -> std::result::Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("forced serialization failure"))
        }
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("auth.json");
    std::fs::write(&path, "complete auth metadata").expect("seed destination");

    write_auth_metadata_atomically(&path, &FailingSerialize)
        .expect_err("serialization should fail");

    assert_eq!(
        std::fs::read_to_string(&path).expect("read destination"),
        "complete auth metadata"
    );
    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("read auth directory")
            .count(),
        1
    );
}

#[test]
fn atomic_auth_write_replaces_destination_privately() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("auth.json");
    std::fs::write(&path, "stale auth metadata").expect("seed destination");

    write_auth_metadata_atomically(&path, &serde_json::json!({"deviceId": "device-1"}))
        .expect("atomic auth write");

    let content = std::fs::read_to_string(&path).expect("read destination");
    assert!(content.contains("device-1"));
    assert!(!content.contains("stale auth metadata"));
    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("read auth directory")
            .count(),
        1
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path)
                .expect("auth metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn colliding_legacy_backend_names_have_distinct_auth_namespaces() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dotted = auth_path_for_api_base_url(dir.path(), "https://api.statsai.dev");
    let dashed = auth_path_for_api_base_url(dir.path(), "https://api-statsai.dev");

    assert_eq!(
        legacy_scoped_auth_path(dir.path(), "https://api.statsai.dev"),
        legacy_scoped_auth_path(dir.path(), "https://api-statsai.dev")
    );
    assert_ne!(dotted, dashed);
    assert_ne!(
        keyring_backend_key("https://api.statsai.dev"),
        keyring_backend_key("https://api-statsai.dev")
    );
}

#[test]
fn scoped_auth_file_with_mismatched_backend_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let requested_backend = "https://api.statsai.dev";
    let path = auth_path_for_api_base_url(dir.path(), requested_backend);
    let credentials = AuthCredentials {
        backend: Some("cloudflare".to_string()),
        api_base_url: Some("https://attacker.invalid".to_string()),
        cloudflare_refresh_token: Some("must-not-load".to_string()),
        cloudflare_refresh_expires_at_secs: 0,
        cloudflare_access_token: None,
        cloudflare_access_expires_at_secs: 0,
        device_id: Some("device-1".to_string()),
    };
    write_credentials(&path, &credentials).expect("write mismatched credentials");

    assert!(auth_record_from_file(dir.path(), requested_backend)
        .expect("read auth record")
        .is_none());
}

#[test]
fn scoped_auth_file_without_embedded_backend_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let requested_backend = "https://api.statsai.dev";
    let path = auth_path_for_api_base_url(dir.path(), requested_backend);
    let credentials = AuthCredentials {
        backend: Some("cloudflare".to_string()),
        api_base_url: None,
        cloudflare_refresh_token: Some("must-not-load".to_string()),
        cloudflare_refresh_expires_at_secs: 0,
        cloudflare_access_token: None,
        cloudflare_access_expires_at_secs: 0,
        device_id: Some("device-1".to_string()),
    };
    write_credentials(&path, &credentials).expect("write credentials");

    assert!(auth_record_from_file(dir.path(), requested_backend)
        .expect("read auth record")
        .is_none());
}

#[test]
fn colliding_legacy_auth_file_is_not_migrated_for_another_backend() {
    let dir = tempfile::tempdir().expect("tempdir");
    let requested_backend = "https://api.statsai.dev";
    let colliding_backend = "https://api-statsai.dev";
    let legacy_path = legacy_scoped_auth_path(dir.path(), colliding_backend);
    let credentials = AuthCredentials {
        backend: Some("cloudflare".to_string()),
        api_base_url: Some(colliding_backend.to_string()),
        cloudflare_refresh_token: Some("must-not-migrate".to_string()),
        cloudflare_refresh_expires_at_secs: 0,
        cloudflare_access_token: None,
        cloudflare_access_expires_at_secs: 0,
        device_id: Some("device-1".to_string()),
    };
    write_credentials(&legacy_path, &credentials).expect("write legacy credentials");

    assert!(auth_record_from_file(dir.path(), requested_backend)
        .expect("read auth record")
        .is_none());
    assert!(!auth_path_for_api_base_url(dir.path(), requested_backend).exists());
}

#[test]
fn legacy_scoped_auth_record_hydrates_before_session_check() {
    let dir = tempfile::tempdir().expect("tempdir");
    let api_base_url = "https://api.statsai.dev";
    let legacy_path = legacy_scoped_auth_path(dir.path(), api_base_url);
    let redacted = AuthCredentials {
        backend: Some("cloudflare".to_string()),
        api_base_url: Some(api_base_url.to_string()),
        cloudflare_refresh_token: None,
        cloudflare_refresh_expires_at_secs: 0,
        cloudflare_access_token: None,
        cloudflare_access_expires_at_secs: 0,
        device_id: Some("device-1".to_string()),
    };
    write_credentials(&legacy_path, &redacted).expect("write legacy credentials");

    let record = auth_record_from_file_with_loader(dir.path(), api_base_url, |path| {
        let mut hydrated = load_credentials_from_file(path)?;
        hydrated.cloudflare_refresh_token = Some("legacy-refresh".to_string());
        Ok(hydrated)
    })
    .expect("migrate legacy auth record");

    let (path, credentials) = record.expect("hydrated auth record");
    assert_eq!(path, auth_path_for_api_base_url(dir.path(), api_base_url));
    assert_eq!(
        credentials.cloudflare_refresh_token.as_deref(),
        Some("legacy-refresh")
    );
    assert!(!legacy_path.exists());
}

#[test]
fn keyring_session_requires_matching_embedded_backend() {
    let session = KeyringSession {
        api_base_url: Some("https://api.statsai.dev".to_string()),
        refresh_token: Some("refresh-token".to_string()),
        access_token: None,
    };

    assert!(keyring_session_matches_backend(
        &session,
        "https://api.statsai.dev/"
    ));
    assert!(!keyring_session_matches_backend(
        &session,
        "https://api-statsai.dev"
    ));
    assert!(!keyring_session_matches_backend(
        &KeyringSession {
            api_base_url: None,
            ..session
        },
        "https://api.statsai.dev"
    ));
}

#[test]
fn legacy_keyring_session_hydrates_validated_upgrade_credentials() {
    let mut credentials = AuthCredentials {
        backend: Some("cloudflare".to_string()),
        api_base_url: Some("https://api.statsai.dev".to_string()),
        cloudflare_refresh_token: None,
        cloudflare_refresh_expires_at_secs: 0,
        cloudflare_access_token: None,
        cloudflare_access_expires_at_secs: 0,
        device_id: Some("device-1".to_string()),
    };
    let legacy_session = KeyringSession {
        api_base_url: None,
        refresh_token: Some("legacy-refresh".to_string()),
        access_token: Some("legacy-access".to_string()),
    };

    let migrated = hydrate_credentials_from_sessions(
        &mut credentials,
        "https://api.statsai.dev",
        None,
        Some(legacy_session),
        None,
    );

    assert!(migrated);
    assert_eq!(
        credentials.cloudflare_refresh_token.as_deref(),
        Some("legacy-refresh")
    );
    assert_eq!(
        credentials.cloudflare_access_token.as_deref(),
        Some("legacy-access")
    );
}

#[test]
fn split_legacy_keyring_tokens_hydrate_validated_upgrade_credentials() {
    let mut credentials = AuthCredentials {
        backend: Some("cloudflare".to_string()),
        api_base_url: Some("https://api.statsai.dev".to_string()),
        cloudflare_refresh_token: None,
        cloudflare_refresh_expires_at_secs: 0,
        cloudflare_access_token: None,
        cloudflare_access_expires_at_secs: 0,
        device_id: Some("device-1".to_string()),
    };
    let split_session = KeyringSession {
        api_base_url: None,
        refresh_token: Some("split-refresh".to_string()),
        access_token: Some("split-access".to_string()),
    };

    let migrated = hydrate_credentials_from_sessions(
        &mut credentials,
        "https://api.statsai.dev",
        None,
        None,
        Some(split_session),
    );

    assert!(migrated);
    assert_eq!(
        credentials.cloudflare_refresh_token.as_deref(),
        Some("split-refresh")
    );
    assert_eq!(
        credentials.cloudflare_access_token.as_deref(),
        Some("split-access")
    );
    assert_eq!(
        legacy_refresh_keyring_account("https://api.statsai.dev"),
        "cf-refresh-https___api_statsai_dev"
    );
    assert_eq!(
        legacy_access_keyring_account("https://api.statsai.dev"),
        "cf-access-https___api_statsai_dev"
    );
    assert_eq!(
        legacy_refresh_keyring_account("https://api.statsai.dev/"),
        "cf-refresh-https___api_statsai_dev_"
    );
}

#[test]
fn legacy_local_cloudflare_session_migrates_to_backend_scoped_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let legacy_path = legacy_auth_path(dir.path());
    let credentials = AuthCredentials {
        backend: Some("cloudflare".to_string()),
        api_base_url: Some("http://127.0.0.1:8787".to_string()),
        cloudflare_refresh_token: Some("refresh-token".to_string()),
        cloudflare_refresh_expires_at_secs: 0,
        cloudflare_access_token: Some("access-token".to_string()),
        cloudflare_access_expires_at_secs: 123,
        device_id: Some("device-1".to_string()),
    };
    write_credentials(&legacy_path, &credentials).expect("write legacy creds");

    let record = auth_record_for_backend(dir.path(), "http://127.0.0.1:8787").expect("auth record");
    let Some((path, loaded)) = record else {
        panic!("expected migrated auth record");
    };

    assert_eq!(
        path,
        auth_path_for_api_base_url(dir.path(), "http://127.0.0.1:8787")
    );
    assert!(path.exists());
    assert_eq!(loaded.device_id.as_deref(), Some("device-1"));
}

#[test]
fn local_session_does_not_bleed_into_other_backend() {
    let dir = tempfile::tempdir().expect("tempdir");
    let legacy_path = legacy_auth_path(dir.path());
    let credentials = AuthCredentials {
        backend: Some("cloudflare".to_string()),
        api_base_url: Some("http://127.0.0.1:8787".to_string()),
        cloudflare_refresh_token: Some("refresh-token".to_string()),
        cloudflare_refresh_expires_at_secs: 0,
        cloudflare_access_token: Some("access-token".to_string()),
        cloudflare_access_expires_at_secs: 123,
        device_id: Some("device-1".to_string()),
    };
    write_credentials(&legacy_path, &credentials).expect("write legacy creds");

    let record =
        auth_record_for_backend(dir.path(), "https://api.example.com").expect("auth record");
    assert!(record.is_none());
}

#[test]
fn token_refresh_sends_current_collector_metadata() {
    let rotation_id = "a".repeat(64);
    let json = token_refresh_request_payload("refresh-token", &rotation_id);
    assert_eq!(json["refreshToken"].as_str(), Some("refresh-token"));
    assert_eq!(json["rotationId"].as_str(), Some(rotation_id.as_str()));
    assert_eq!(
        json["collectorVersion"].as_str(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(json["platform"].as_str(), Some(std::env::consts::OS));
}

#[test]
fn refresh_rotation_ids_have_256_bits_of_hex_encoded_entropy() {
    let rotation_id = generate_refresh_rotation_id().expect("rotation id");
    assert_eq!(rotation_id.len(), 64);
    assert!(rotation_id.bytes().all(|byte| byte.is_ascii_hexdigit()));
}

#[test]
fn pending_refresh_rotation_survives_invocations_until_credentials_advance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let auth_path = dir.path().join("auth-test.json");

    let first = {
        let _lock = acquire_auth_refresh_lock(&auth_path).expect("first refresh lock");
        load_or_create_pending_refresh_rotation(&auth_path, "refresh-token-a")
            .expect("first rotation")
    };
    let retry = {
        let _lock = acquire_auth_refresh_lock(&auth_path).expect("retry refresh lock");
        load_or_create_pending_refresh_rotation(&auth_path, "refresh-token-a")
            .expect("retried rotation")
    };
    let next_credentials = {
        let _lock = acquire_auth_refresh_lock(&auth_path).expect("next refresh lock");
        load_or_create_pending_refresh_rotation(&auth_path, "refresh-token-b")
            .expect("next rotation")
    };

    assert_eq!(retry, first);
    assert_ne!(next_credentials, first);
    clear_pending_refresh_rotation(&auth_path).expect("clear pending rotation");
    assert!(!pending_refresh_rotation_path(&auth_path).exists());
}

#[test]
fn concurrent_refresh_processes_share_one_pending_rotation() {
    use std::sync::{Arc, Barrier};

    let dir = tempfile::tempdir().expect("tempdir");
    let auth_path = Arc::new(dir.path().join("auth-test.json"));
    let barrier = Arc::new(Barrier::new(8));
    let threads = (0..8)
        .map(|_| {
            let auth_path = Arc::clone(&auth_path);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let _lock = acquire_auth_refresh_lock(&auth_path).expect("refresh lock");
                load_or_create_pending_refresh_rotation(&auth_path, "refresh-token")
                    .expect("pending rotation")
            })
        })
        .collect::<Vec<_>>();
    let rotation_ids = threads
        .into_iter()
        .map(|thread| thread.join().expect("refresh thread"))
        .collect::<Vec<_>>();

    assert!(rotation_ids
        .iter()
        .all(|rotation_id| rotation_id == &rotation_ids[0]));
}

#[test]
fn token_refresh_retries_one_transport_failure() {
    let mut attempts = 0;
    let result = retry_once_if(
        || {
            attempts += 1;
            if attempts == 1 {
                Err("transport")
            } else {
                Ok("response")
            }
        },
        |error| *error == "transport",
    );

    assert_eq!(result, Ok("response"));
    assert_eq!(attempts, 2);
}

#[test]
fn token_refresh_does_not_retry_http_status_failures() {
    let mut attempts = 0;
    let result: Result<(), &str> = retry_once_if(
        || {
            attempts += 1;
            Err("http-status")
        },
        |error| *error == "transport",
    );

    assert_eq!(result, Err("http-status"));
    assert_eq!(attempts, 1);
}

#[test]
fn preferred_auth_device_id_reuses_stored_backend_device_id_when_sidecar_is_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let api_base_url = "https://api.example.com";
    let auth_path = auth_path_for_api_base_url(dir.path(), api_base_url);
    let credentials = AuthCredentials {
        backend: Some("cloudflare".to_string()),
        api_base_url: Some(api_base_url.to_string()),
        cloudflare_refresh_token: Some("refresh-token".to_string()),
        cloudflare_refresh_expires_at_secs: 0,
        cloudflare_access_token: Some("access-token".to_string()),
        cloudflare_access_expires_at_secs: 123,
        device_id: Some("device-1".to_string()),
    };
    write_credentials(&auth_path, &credentials).expect("write auth credentials");

    let preferred = preferred_auth_device_id(dir.path(), api_base_url);

    assert_eq!(preferred, "device-1");
    let sidecar = std::fs::read_to_string(auth_device_id_path_for_api_base_url(
        dir.path(),
        api_base_url,
    ))
    .expect("sidecar device id");
    assert_eq!(sidecar.trim(), "device-1");
}

#[test]
fn preferred_auth_device_id_falls_back_when_auth_record_is_corrupt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let api_base_url = "https://api.example.com";
    let auth_path = auth_path_for_api_base_url(dir.path(), api_base_url);
    std::fs::write(&auth_path, "{not-json").expect("write corrupt auth file");

    let preferred = preferred_auth_device_id_with_fallback(dir.path(), api_base_url, || {
        "fallback-device".to_string()
    });

    assert_eq!(preferred, "fallback-device");
}

#[test]
fn with_device_id_retry_retries_after_invalid_device_id() {
    let mut seen_device_ids = Vec::new();
    let result = with_device_id_retry("remembered-device", "retry exhausted", |device_id| {
        seen_device_ids.push(device_id.to_string());
        if seen_device_ids.len() == 1 {
            Err(DeviceSessionRequestError::InvalidDeviceId)
        } else {
            Ok("ok")
        }
    })
    .expect("retry succeeds");

    assert_eq!(result, "ok");
    assert_eq!(seen_device_ids.len(), 2);
    assert_eq!(seen_device_ids[0], "remembered-device");
    assert_ne!(seen_device_ids[1], "remembered-device");
}

#[test]
fn with_device_id_retry_propagates_fatal_errors() {
    let error = with_device_id_retry("remembered-device", "retry exhausted", |_device_id| {
        Err::<(), _>(DeviceSessionRequestError::Fatal(anyhow::anyhow!(
            "fatal problem"
        )))
    })
    .expect_err("fatal error should propagate");

    assert!(error.to_string().contains("fatal problem"));
}
