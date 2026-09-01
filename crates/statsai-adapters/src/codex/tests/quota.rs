use super::*;

#[test]
fn codex_quota_parser_is_anchored_and_preserves_modern_status_fields() {
    let adapter = CodexAdapter;
    let source = codex_source_for_root(
        &adapter,
        Path::new("/tmp/codex-home"),
        LocationOrigin::Configured,
    );
    let observed_at = Utc.with_ymd_and_hms(2026, 8, 20, 12, 0, 0).unwrap();
    let value = serde_json::json!({
        "timestamp": observed_at,
        "type": "event_msg",
        "payload": {
            "type": "token_count",
            "info": {"last_token_usage": {"total_tokens": 0}},
            "rate_limits": {
                "limit_id": "codex_subscription",
                "plan_type": "pro",
                "individual_limit": null,
                "spend_control_state": "allowed",
                "reached_type": "weekly",
                "primary": {
                    "used_percent": 12.5,
                    "window_minutes": 10080,
                    "resets_at": 1787832000
                },
                "credits": {
                    "has_credits": true,
                    "unlimited": false,
                    "balance": "0012.5000"
                }
            }
        }
    });
    let record = codex_quota_observation(
        &source,
        Path::new("/tmp/codex-home/sessions/thread.jsonl"),
        7,
        observed_at,
        Some(UsageCounts::default()),
        &value,
    )
    .expect("quota observation");

    assert_eq!(record.windows.len(), 1);
    assert_eq!(record.windows[0].provider_slot, "primary");
    assert_eq!(record.windows[0].window_minutes, 10_080);
    assert_eq!(
        record.windows[0].limit_id.as_deref(),
        Some("codex_subscription")
    );
    assert_eq!(record.observation.status.plan_type.as_deref(), Some("pro"));
    assert_eq!(
        record.observation.status.credits.balance.as_deref(),
        Some("12.5")
    );
    assert_eq!(
        record.observation.status.credits.balance_raw,
        Some(Value::String("0012.5000".to_string()))
    );
    assert_eq!(record.observation.usage_link_kind, QuotaUsageLinkKind::None);

    let nested_as_text = serde_json::json!({
        "type": "event_msg",
        "payload": {
            "type": "user_message",
            "message": value.to_string()
        }
    });
    assert!(codex_quota_observation(
        &source,
        Path::new("/tmp/thread.jsonl"),
        1,
        observed_at,
        None,
        &nested_as_text,
    )
    .is_none());
}

#[test]
fn codex_quota_parser_requires_integer_reset_epochs_and_leniently_reads_balances() {
    let adapter = CodexAdapter;
    let source = codex_source_for_root(
        &adapter,
        Path::new("/tmp/codex-home"),
        LocationOrigin::Configured,
    );
    let observed_at = Utc.with_ymd_and_hms(2026, 8, 20, 12, 0, 0).unwrap();
    for (balance, normalized) in [
        (serde_json::json!(14.25), Some("14.25")),
        (serde_json::json!("1.25e-3"), Some("0.00125")),
        (Value::Null, None),
    ] {
        let value = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "rate_limits": {
                    "primary": {
                        "used_percent": 1,
                        "window_minutes": 300,
                        "resets_at": "1787832000"
                    },
                    "credits": {"balance": balance}
                }
            }
        });
        let record = codex_quota_observation(
            &source,
            Path::new("/tmp/thread.jsonl"),
            1,
            observed_at,
            None,
            &value,
        )
        .expect("structural quota payload");
        assert!(record.windows.is_empty(), "string epochs are invalid");
        assert_eq!(
            record.observation.status.credits.balance.as_deref(),
            normalized
        );
    }
    assert!(codex_quota_observation(
        &source,
        Path::new("/tmp/thread.jsonl"),
        1,
        observed_at,
        None,
        &serde_json::json!({
            "type": "event_msg",
            "payload": {"type": "token_count", "rate_limits": "malformed"}
        }),
    )
    .is_none());
}

#[test]
fn codex_quota_links_consumed_samples_to_turn_events_and_preserves_zero_samples() {
    let directory = tempfile::tempdir().expect("tempdir");
    let root = directory.path().join("codex");
    let sessions = root.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let path = sessions.join("thread.jsonl");
    let mut fixture = File::create(&path).expect("fixture");
    for value in [
        serde_json::json!({
            "timestamp": "2026-08-20T12:00:00Z",
            "type": "event_msg",
            "payload": {"type": "task_started"}
        }),
        serde_json::json!({
            "timestamp": "2026-08-20T12:00:01Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {"input_tokens": 10, "output_tokens": 5},
                    "total_token_usage": {"input_tokens": 10, "output_tokens": 5}
                },
                "rate_limits": {
                    "primary": {
                        "used_percent": 10,
                        "window_minutes": 10080,
                        "resets_at": 1787832000
                    }
                }
            }
        }),
        serde_json::json!({
            "timestamp": "2026-08-20T12:00:02Z",
            "type": "event_msg",
            "payload": {"type": "task_complete"}
        }),
        serde_json::json!({
            "timestamp": "2026-08-20T12:00:03Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {"total_tokens": 0},
                    "total_token_usage": {"input_tokens": 10, "output_tokens": 5}
                },
                "rate_limits": {
                    "primary": {
                        "used_percent": 11,
                        "window_minutes": 10080,
                        "resets_at": 1787832000
                    }
                }
            }
        }),
    ] {
        writeln!(fixture, "{value}").expect("write fixture");
    }
    drop(fixture);
    let source = codex_source_for_root(&CodexAdapter, &root, LocationOrigin::Configured);
    let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.quota_observations.len(), 2);
    assert_eq!(
        scan.quota_observations[0].observation.usage_link_kind,
        QuotaUsageLinkKind::TurnEvent
    );
    assert_eq!(
        scan.quota_observations[0].observation.usage_event_id,
        Some(scan.events[0].event_id.clone())
    );
    assert_eq!(
        scan.quota_observations[1]
            .observation
            .usage_sample
            .as_ref()
            .map(UsageCounts::computed_total),
        Some(0)
    );
    assert_eq!(
        scan.quota_observations[1].observation.usage_link_kind,
        QuotaUsageLinkKind::None
    );
    assert!(scan.quota_observations[1]
        .observation
        .usage_event_id
        .is_none());
}
