use super::logout::logout_backend;
use super::session::{with_device_id_retry, DeviceSessionRequestError};
use super::*;
use std::cell::Cell;
use std::io::Write;

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

#[test]
fn oauth_callback_keeps_state_checks_and_header_limits() {
    use std::io::Write;
    use std::net::TcpStream;
    use std::time::Duration;

    let server = statsai_daemon::http::Server::http("127.0.0.1:0").expect("bind");
    let address = server.server_addr().to_string();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let code =
            super::session::listen_for_callback(&server, "state-expected").expect("callback");
        sender.send(code).expect("send code");
    });

    let mut oversized = TcpStream::connect(&address).expect("connect");
    oversized
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    oversized
        .write_all(b"GET /callback?code=early&state=state-expected HTTP/1.1\r\n")
        .expect("write");
    oversized
        .write_all(&vec![b'h'; statsai_daemon::http::MAX_HEADER_LINE_BYTES + 8])
        .expect("write");
    oversized.write_all(b"\r\n\r\n").expect("write");
    let rejected = read_socket(&mut oversized);
    assert!(rejected.contains("431"), "{rejected}");

    let mut invalid = TcpStream::connect(&address).expect("connect");
    invalid
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    invalid
        .write_all(b"GET /callback?code=nope&state=wrong HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .expect("write");
    let rejected = read_socket(&mut invalid);
    assert!(rejected.contains("400"), "{rejected}");
    assert!(receiver.try_recv().is_err());

    let mut valid = TcpStream::connect(&address).expect("connect");
    valid
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    valid
        .write_all(
            b"GET /callback?code=abc%20123&state=state-expected HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .expect("write");
    let accepted = read_socket(&mut valid);
    assert!(accepted.contains("200"), "{accepted}");
    assert!(accepted.contains("Device linked"), "{accepted}");
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(2)).expect("code"),
        "abc 123"
    );
}

fn read_socket(stream: &mut std::net::TcpStream) -> String {
    use std::io::Read;
    use std::time::{Duration, Instant};

    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => buffer.extend_from_slice(&chunk[..count]),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                if !buffer.is_empty() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

#[test]
fn oversized_token_refresh_does_not_replace_credentials() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        read_http_headers(&mut stream);
        let body = vec![b'x'; statsai_core::JSON_RESPONSE_LIMIT_AUTH + 1];
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(header.as_bytes()).expect("header");
        stream.write_all(&body).ok();
    });
    let api = format!("http://{address}");
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("auth.json");
    let mut credentials = AuthCredentials {
        backend: Some("cloudflare".to_string()),
        api_base_url: Some(api.clone()),
        cloudflare_refresh_token: Some("refresh-old".to_string()),
        cloudflare_refresh_expires_at_secs: 10,
        cloudflare_access_token: Some("access-old".to_string()),
        cloudflare_access_expires_at_secs: 10,
        device_id: Some("device-old".to_string()),
    };
    write_credentials(&path, &credentials).expect("seed");
    let before = std::fs::read(&path).expect("read seed");
    let error = refresh_cloudflare_access_token(&path, &mut credentials, &api)
        .expect_err("oversized refresh");
    let message = format!("{error:#}");
    assert!(message.contains("byte limit"), "{message}");
    assert_eq!(std::fs::read(&path).expect("read after"), before);
    assert_eq!(
        credentials.cloudflare_refresh_token.as_deref(),
        Some("refresh-old")
    );
    assert_eq!(
        credentials.cloudflare_access_token.as_deref(),
        Some("access-old")
    );
}

#[test]
fn malformed_token_refresh_does_not_replace_credentials() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        read_http_headers(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1\r\nConnection: close\r\n\r\n{")
            .expect("write");
    });
    let api = format!("http://{address}");
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("auth.json");
    let mut credentials = AuthCredentials {
        backend: Some("cloudflare".to_string()),
        api_base_url: Some(api.clone()),
        cloudflare_refresh_token: Some("refresh-old".to_string()),
        cloudflare_refresh_expires_at_secs: 10,
        cloudflare_access_token: Some("access-old".to_string()),
        cloudflare_access_expires_at_secs: 10,
        device_id: Some("device-old".to_string()),
    };
    write_credentials(&path, &credentials).expect("seed");
    let before = std::fs::read(&path).expect("read seed");
    let error = refresh_cloudflare_access_token(&path, &mut credentials, &api)
        .expect_err("malformed refresh");
    assert!(
        format!("{error:#}").contains("parse JSON") || format!("{error:#}").contains("expected"),
        "{error:#}"
    );
    assert_eq!(std::fs::read(&path).expect("read after"), before);
    assert_eq!(
        credentials.cloudflare_access_token.as_deref(),
        Some("access-old")
    );
}

fn read_http_headers(stream: &mut std::net::TcpStream) {
    use std::io::Read;
    let mut seen = Vec::new();
    let mut byte = [0_u8; 1];
    while stream.read(&mut byte).unwrap_or(0) == 1 {
        seen.push(byte[0]);
        if seen.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&seen);
    let mut length = 0_usize;
    for line in text.lines() {
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut left = length;
    let mut trash = [0_u8; 1024];
    while left > 0 {
        let chunk = left.min(trash.len());
        let count = stream.read(&mut trash[..chunk]).unwrap_or(0);
        if count == 0 {
            break;
        }
        left -= count;
    }
}

#[cfg(target_os = "linux")]
#[test]
fn linux_secret_service_negotiates_an_encrypted_session() {
    let _isolated = IsolatedSecretService::start();
    let entry = session_entry("http://127.0.0.1:9").expect("session entry");
    entry
        .set_password("encrypted-session-secret")
        .expect("store session");
    assert_eq!(
        entry.get_password().expect("read session"),
        "encrypted-session-secret"
    );
    let log = _isolated.finish();
    let algorithms = open_session_algorithms(&log);
    assert!(
        algorithms
            .iter()
            .any(|algorithm| algorithm == "dh-ietf1024-sha256-aes128-cbc-pkcs7"),
        "{log}"
    );
    assert!(
        algorithms.iter().all(|algorithm| algorithm != "plain"),
        "{log}"
    );
}

#[cfg(target_os = "linux")]
struct IsolatedSecretService {
    dbus_pid: u32,
    keyring_pid: u32,
    monitor: std::process::Child,
    log_path: std::path::PathBuf,
    previous_env: [(&'static str, Option<String>); 3],
    _directory: tempfile::TempDir,
}

#[cfg(target_os = "linux")]
impl IsolatedSecretService {
    fn start() -> Self {
        let directory = tempfile::tempdir().expect("isolated dbus dir");
        let runtime = directory.path().join("runtime");
        let data = directory.path().join("data");
        std::fs::create_dir_all(&runtime).expect("runtime");
        std::fs::create_dir_all(&data).expect("data");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
                .expect("runtime permissions");
        }
        let socket = directory.path().join("bus");
        let output = std::process::Command::new("dbus-daemon")
            .args([
                "--session",
                "--fork",
                "--nopidfile",
                "--print-address=1",
                "--print-pid=1",
                &format!("--address=unix:path={}", socket.display()),
            ])
            .output()
            .expect("start dbus-daemon");
        assert!(
            output.status.success(),
            "dbus-daemon failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).expect("dbus output");
        let mut lines = text.lines();
        let address = lines.next().expect("dbus address").to_string();
        let dbus_pid = lines
            .next()
            .and_then(|line| line.trim().parse().ok())
            .expect("dbus pid");
        let previous_env = [
            set_isolated_env("DBUS_SESSION_BUS_ADDRESS", &address),
            set_isolated_env("XDG_RUNTIME_DIR", &runtime.to_string_lossy()),
            set_isolated_env("XDG_DATA_HOME", &data.to_string_lossy()),
        ];
        std::fs::create_dir_all(runtime.join("keyring")).expect("keyring control dir");
        let started = std::process::Command::new("gnome-keyring-daemon")
            .args(["--start", "--components=secrets", "--daemonize"])
            .output()
            .expect("start gnome-keyring");
        assert!(
            started.status.success(),
            "gnome-keyring start failed: {}\n{}",
            String::from_utf8_lossy(&started.stdout),
            String::from_utf8_lossy(&started.stderr)
        );

        let keyring_pid = wait_for_secret_service_pid();
        let mut unlock = std::process::Command::new("gnome-keyring-daemon")
            .arg("--unlock")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn gnome-keyring unlock");
        unlock
            .stdin
            .take()
            .expect("unlock stdin")
            .write_all(b"statsai-test")
            .expect("password");
        let unlock = unlock.wait_with_output().expect("unlock");
        assert!(
            unlock.status.success(),
            "gnome-keyring unlock failed: {}",
            String::from_utf8_lossy(&unlock.stderr)
        );
        alias_unlocked_collection_as_default();
        let log_path = directory.path().join("monitor.log");
        let log_file = std::fs::File::create(&log_path).expect("monitor log");
        let monitor = std::process::Command::new("dbus-monitor")
            .arg("--session")
            .stdout(log_file)
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("dbus-monitor");
        std::thread::sleep(std::time::Duration::from_millis(200));
        Self {
            dbus_pid,
            keyring_pid,
            monitor,
            log_path,
            previous_env,
            _directory: directory,
        }
    }

    fn finish(mut self) -> String {
        let _ = self.monitor.kill();
        let _ = self.monitor.wait();
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }
}

#[cfg(target_os = "linux")]
impl Drop for IsolatedSecretService {
    fn drop(&mut self) {
        let _ = self.monitor.kill();
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &self.keyring_pid.to_string()])
            .status();
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &self.dbus_pid.to_string()])
            .status();
        for (key, previous) in &self.previous_env {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn set_isolated_env(key: &'static str, value: &str) -> (&'static str, Option<String>) {
    let previous = std::env::var(key).ok();
    std::env::set_var(key, value);
    (key, previous)
}

#[cfg(target_os = "linux")]
#[cfg(target_os = "linux")]
fn alias_unlocked_collection_as_default() {
    let listed = std::process::Command::new("dbus-send")
        .args([
            "--session",
            "--print-reply",
            "--dest=org.freedesktop.secrets",
            "/org/freedesktop/secrets",
            "org.freedesktop.DBus.Properties.Get",
            "string:org.freedesktop.Secret.Service",
            "string:Collections",
        ])
        .output()
        .expect("list secret collections");
    let text = String::from_utf8_lossy(&listed.stdout);
    let mut paths = text
        .lines()
        .filter_map(|line| {
            let start = line.find('"')?;
            let rest = &line[start + 1..];
            let end = rest.find('"')?;
            let path = &rest[..end];
            path.starts_with("/org/freedesktop/secrets/collection/")
                .then(|| path.to_string())
        })
        .collect::<Vec<_>>();
    paths.sort_by_key(|path| !path.ends_with("/session"));
    let Some(path) = paths.first() else {
        panic!("secret service has no collection:\n{text}");
    };
    let aliased = std::process::Command::new("dbus-send")
        .args([
            "--session",
            "--print-reply",
            "--dest=org.freedesktop.secrets",
            "/org/freedesktop/secrets",
            "org.freedesktop.Secret.Service.SetAlias",
            "string:default",
            &format!("objpath:{path}"),
        ])
        .output()
        .expect("alias default collection");
    assert!(
        aliased.status.success(),
        "alias default collection failed: {}\n{}",
        String::from_utf8_lossy(&aliased.stdout),
        String::from_utf8_lossy(&aliased.stderr)
    );
}

#[cfg(target_os = "linux")]
fn wait_for_secret_service_pid() -> u32 {
    let started = std::time::Instant::now();
    let mut last = String::new();
    while started.elapsed() < std::time::Duration::from_secs(5) {
        match secret_service_pid() {
            Ok(pid) => return pid,
            Err(error) => last = error,
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("secret service did not register: {last}");
}

#[cfg(target_os = "linux")]
fn secret_service_pid() -> Result<u32, String> {
    let output = std::process::Command::new("dbus-send")
        .args([
            "--session",
            "--dest=org.freedesktop.DBus",
            "--print-reply",
            "--type=method_call",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.GetConnectionUnixProcessID",
            "string:org.freedesktop.secrets",
        ])
        .output()
        .map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace()
        .find_map(|token| token.parse::<u32>().ok())
        .ok_or_else(|| format!("{}\n{}", text, String::from_utf8_lossy(&output.stderr)))
}

#[cfg(target_os = "linux")]
fn open_session_algorithms(log: &str) -> Vec<String> {
    let mut algorithms = Vec::new();
    let mut lines = log.lines();
    while let Some(line) = lines.next() {
        if !line.contains("member=OpenSession") {
            continue;
        }
        let Some(next) = lines.next() else {
            break;
        };
        if let Some(start) = next.find('"') {
            if let Some(end) = next[start + 1..].find('"') {
                algorithms.push(next[start + 1..start + 1 + end].to_string());
            }
        }
    }
    algorithms
}
