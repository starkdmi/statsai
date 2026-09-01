use super::*;

#[test]
fn grok_build_prices_unified_log_inferences_independently() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-mixed");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::create_dir_all(dir.path().join("logs")).expect("logs dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-mixed", "cwd": dir.path()},
            "updated_at": "2026-08-16T18:39:58Z",
            "current_model_id": "grok-4.6-build",
            "chat_format_version": 1
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(
        dir.path().join("logs/unified.jsonl"),
        [
            serde_json::json!({
                "ts": "2026-08-16T18:32:52.141Z",
                "sid": "session-mixed",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 100_000,
                    "cached_prompt_tokens": 40_000,
                    "completion_tokens": 10_000,
                    "reasoning_tokens": 0
                }
            })
            .to_string(),
            serde_json::json!({
                "ts": "2026-08-16T18:33:03.314Z",
                "sid": "session-mixed",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 200_000,
                    "cached_prompt_tokens": 80_000,
                    "completion_tokens": 10_000,
                    "reasoning_tokens": 0
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("unified log");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_grok_build_source(&GrokBuildAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.summaries.len(), 1);
    let summary = &scan.summaries[0];
    let model = summary.model.as_ref().expect("model");
    assert_eq!(model.name.as_deref(), Some("grok-4.6-build"));
    assert_eq!(model.normalized_name.as_deref(), Some("grok-4.6"));
    assert_eq!(summary.usage.input_tokens, Some(180_000));
    assert_eq!(summary.usage.cache_read_tokens, Some(120_000));
    assert_eq!(summary.usage.output_tokens, Some(20_000));
    assert_eq!(summary.usage.reasoning_tokens, None);
    assert_eq!(summary.usage.requests, Some(2));
    assert_eq!(
        summary.cost.estimated_api_equivalent_micro_usd,
        Some(880_000)
    );
    assert_eq!(summary.cost.estimated_api_equivalent_usd, Some(88));
    assert_eq!(summary.cost.confidence, Confidence::Medium);
    assert_eq!(
        summary.cost.pricing_source.as_deref(),
        Some("xai_api_pricing:grok-4.6:unified_log_inference_usage")
    );
    assert_eq!(
        summary
            .metadata
            .summary_version
            .as_deref()
            .map(|value| value.contains("inference_rows=2;usage_source=unified_log")),
        Some(true)
    );
}

#[test]
fn grok_build_prices_mixed_model_session_from_prompt_model_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-mixed-models");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::create_dir_all(dir.path().join("logs")).expect("logs dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-mixed-models", "cwd": dir.path()},
            "updated_at": "2026-08-16T18:39:58Z",
            "current_model_id": "grok-4.6-build",
            "chat_format_version": 1
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(
        session.join("signals.json"),
        serde_json::json!({
            "modelsUsed": ["grok-4.5", "grok-4.6"],
            "primaryModelId": "grok-4.6",
            "turnCount": 2
        })
        .to_string(),
    )
    .expect("signals");
    std::fs::write(
        session.join("updates.jsonl"),
        [
            serde_json::json!({
                "timestamp": 1_786_905_120,
                "method": "session/update",
                "params": {
                    "sessionId": "session-mixed-models",
                    "update": {
                        "sessionUpdate": "user_message",
                        "content": {"type": "text", "text": "first"},
                        "_meta": {"modelId": "grok-4.5", "promptIndex": 0}
                    },
                    "_meta": {
                        "eventId": "prompt-0",
                        "agentTimestampMs": 1_786_905_120_000i64
                    }
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": 1_786_905_130,
                "method": "session/update",
                "params": {
                    "sessionId": "session-mixed-models",
                    "update": {
                        "sessionUpdate": "assistant_chunk",
                        "content": {"type": "text", "text": "working"}
                    },
                    "_meta": {
                        "promptId": "req-4.5",
                        "totalTokens": 100_000,
                        "agentTimestampMs": 1_786_905_130_000i64
                    }
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": 1_786_905_180,
                "method": "session/update",
                "params": {
                    "sessionId": "session-mixed-models",
                    "update": {
                        "sessionUpdate": "user_message",
                        "content": {"type": "text", "text": "/model grok-4.6"},
                        "_meta": {"modelId": "grok-4.6", "promptIndex": 1}
                    },
                    "_meta": {
                        "eventId": "prompt-1",
                        "agentTimestampMs": 1_786_905_180_000i64
                    }
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": 1_786_905_200,
                "method": "session/update",
                "params": {
                    "sessionId": "session-mixed-models",
                    "update": {
                        "sessionUpdate": "assistant_chunk",
                        "content": {"type": "text", "text": "switched"}
                    },
                    "_meta": {
                        "promptId": "req-4.6",
                        "totalTokens": 200_000,
                        "agentTimestampMs": 1_786_905_200_000i64
                    }
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("updates");
    std::fs::write(
        session.join("events.jsonl"),
        [
            serde_json::json!({
                "ts": "2026-08-16T18:32:00Z",
                "type": "turn_started",
                "model_id": "grok-4.5",
                "turn_number": 0
            })
            .to_string(),
            serde_json::json!({
                "ts": "2026-08-16T18:32:00.100Z",
                "type": "loop_started",
                "loop_index": 0
            })
            .to_string(),
            serde_json::json!({
                "ts": "2026-08-16T18:33:00Z",
                "type": "turn_started",
                "model_id": "grok-4.6",
                "turn_number": 1
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("events");
    std::fs::write(
        dir.path().join("logs/unified.jsonl"),
        [
            serde_json::json!({
                "ts": "2026-08-16T18:32:10Z",
                "sid": "session-mixed-models",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "loop_index": 1,
                    "prompt_tokens": 100_000,
                    "cached_prompt_tokens": 40_000,
                    "completion_tokens": 10_000,
                    "reasoning_tokens": 0
                }
            })
            .to_string(),
            serde_json::json!({
                "ts": "2026-08-16T18:33:20Z",
                "sid": "session-mixed-models",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "loop_index": 1,
                    "prompt_tokens": 200_000,
                    "cached_prompt_tokens": 80_000,
                    "completion_tokens": 10_000,
                    "reasoning_tokens": 0
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("unified log");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_grok_build_source(&GrokBuildAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.summaries.len(), 1);
    let summary = &scan.summaries[0];
    let model = summary.model.as_ref().expect("model");
    assert_eq!(model.name.as_deref(), Some("grok-4.6-build"));
    assert_eq!(model.normalized_name.as_deref(), Some("grok-4.6"));
    assert_eq!(summary.usage.input_tokens, Some(180_000));
    assert_eq!(summary.usage.cache_read_tokens, Some(120_000));
    assert_eq!(summary.usage.output_tokens, Some(20_000));
    assert_eq!(summary.usage.requests, Some(2));
    // grok-4.5 short (60k/40k/10k @ $2/$0.30/$6) + grok-4.6 long (120k/80k/10k
    // @ $4/$1/$12) = $0.192 + $0.680. Pricing both as current_model_id (4.6)
    // would be $0.880.
    assert_eq!(
        summary.cost.estimated_api_equivalent_micro_usd,
        Some(872_000)
    );
    assert_eq!(summary.cost.estimated_api_equivalent_usd, Some(87));
    assert_eq!(summary.cost.confidence, Confidence::Medium);
    assert_eq!(
        summary.cost.pricing_source.as_deref(),
        Some("xai_api_pricing:mixed:unified_log_inference_usage")
    );
}

#[test]
fn grok_build_keeps_unresolved_mixed_models_unknown() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-mixed-unknown");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::create_dir_all(dir.path().join("logs")).expect("logs dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-mixed-unknown", "cwd": dir.path()},
            "updated_at": "2026-08-16T18:39:58Z",
            "current_model_id": "grok-4.6-build",
            "chat_format_version": 1
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(
        session.join("signals.json"),
        serde_json::json!({
            "modelsUsed": ["grok-4.5", "grok-4.6"],
            "primaryModelId": "grok-4.6",
            "turnCount": 2
        })
        .to_string(),
    )
    .expect("signals");
    std::fs::write(
        dir.path().join("logs/unified.jsonl"),
        [
            serde_json::json!({
                "ts": "2026-08-16T18:32:10Z",
                "sid": "session-mixed-unknown",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 100_000,
                    "cached_prompt_tokens": 40_000,
                    "completion_tokens": 10_000
                }
            })
            .to_string(),
            serde_json::json!({
                "ts": "2026-08-16T18:33:20Z",
                "sid": "session-mixed-unknown",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 200_000,
                    "cached_prompt_tokens": 80_000,
                    "completion_tokens": 10_000
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("unified log");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_grok_build_source(&GrokBuildAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.summaries.len(), 1);
    let summary = &scan.summaries[0];
    assert_eq!(
        summary
            .model
            .as_ref()
            .and_then(|model| model.name.as_deref()),
        Some("grok-4.6-build")
    );
    assert_eq!(summary.usage.requests, Some(2));
    assert_eq!(summary.cost.estimated_api_equivalent_micro_usd, None);
    assert_eq!(summary.cost.estimated_api_equivalent_usd, None);
    assert_eq!(summary.cost.pricing_source.as_deref(), Some("unknown"));
}

#[test]
fn grok_build_keeps_partial_observation_mixed_models_used_unknown() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-partial-mixed");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::create_dir_all(dir.path().join("logs")).expect("logs dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-partial-mixed", "cwd": dir.path()},
            "updated_at": "2026-08-16T18:39:58Z",
            "current_model_id": "grok-4.6-build",
            "chat_format_version": 1
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(
        session.join("signals.json"),
        serde_json::json!({
            "modelsUsed": ["grok-4.5", "grok-4.6"],
            "primaryModelId": "grok-4.6",
            "turnCount": 2
        })
        .to_string(),
    )
    .expect("signals");
    std::fs::write(
        session.join("updates.jsonl"),
        serde_json::json!({
            "timestamp": 1_786_905_120,
            "method": "session/update",
            "params": {
                "sessionId": "session-partial-mixed",
                "update": {
                    "sessionUpdate": "user_message",
                    "content": {"type": "text", "text": "first"},
                    "_meta": {"modelId": "grok-4.5", "promptIndex": 0}
                },
                "_meta": {
                    "eventId": "prompt-0",
                    "agentTimestampMs": 1_786_905_120_000i64
                }
            }
        })
        .to_string(),
    )
    .expect("updates");
    std::fs::write(
        session.join("events.jsonl"),
        serde_json::json!({
            "ts": "2026-08-16T18:32:00Z",
            "type": "turn_started",
            "model_id": "grok-4.5",
            "turn_number": 0
        })
        .to_string(),
    )
    .expect("events");
    std::fs::write(
        dir.path().join("logs/unified.jsonl"),
        [
            serde_json::json!({
                "ts": "2026-08-16T18:32:10Z",
                "sid": "session-partial-mixed",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "loop_index": 1,
                    "prompt_tokens": 100_000,
                    "cached_prompt_tokens": 40_000,
                    "completion_tokens": 10_000
                }
            })
            .to_string(),
            serde_json::json!({
                "ts": "2026-08-16T18:33:20Z",
                "sid": "session-partial-mixed",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "loop_index": 2,
                    "prompt_tokens": 200_000,
                    "cached_prompt_tokens": 80_000,
                    "completion_tokens": 10_000
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("unified log");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_grok_build_source(&GrokBuildAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.summaries.len(), 1);
    let summary = &scan.summaries[0];
    assert_eq!(
        summary
            .model
            .as_ref()
            .and_then(|model| model.name.as_deref()),
        Some("grok-4.6-build")
    );
    assert_eq!(summary.usage.requests, Some(2));
    // A sole grok-4.5 prompt/turn observation must not price the second
    // request as 4.5 when modelsUsed also reports grok-4.6 (that would be
    // $0.840). Attribution is incomplete, so cost stays unknown.
    assert_eq!(summary.cost.estimated_api_equivalent_micro_usd, None);
    assert_eq!(summary.cost.estimated_api_equivalent_usd, None);
    assert_eq!(summary.cost.pricing_source.as_deref(), Some("unknown"));
}

#[test]
fn grok_fixture_session_keeps_grok_4_6_identity_and_is_priced() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grok/basic");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        &root,
        LocationOrigin::Configured,
    );

    let scan = scan_grok_build_source(&GrokBuildAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.summaries.len(), 1);
    let summary = &scan.summaries[0];
    assert_eq!(
        summary
            .model
            .as_ref()
            .and_then(|model| model.name.as_deref()),
        Some("grok-4.6")
    );
    assert_eq!(
        summary
            .model
            .as_ref()
            .and_then(|model| model.normalized_name.as_deref()),
        Some("grok-4.6")
    );
    assert_eq!(summary.usage.input_tokens, Some(15_534));
    assert_eq!(summary.usage.cache_read_tokens, Some(42_624));
    assert_eq!(summary.usage.output_tokens, Some(917));
    assert_eq!(summary.usage.reasoning_tokens, Some(508));
    assert_eq!(summary.usage.requests, Some(3));
    assert_eq!(
        summary.cost.estimated_api_equivalent_micro_usd,
        Some(60_930)
    );
    assert_eq!(
        summary.cost.pricing_source.as_deref(),
        Some("xai_api_pricing:grok-4.6:unified_log_inference_usage")
    );
}

#[test]
fn grok_inference_model_resolution_joins_prompt_model_id_by_timestamp() {
    let first = DateTime::parse_from_rfc3339("2026-08-16T18:32:10Z")
        .expect("first")
        .with_timezone(&Utc);
    let second = DateTime::parse_from_rfc3339("2026-08-16T18:33:20Z")
        .expect("second")
        .with_timezone(&Utc);
    let prompt_models = [
        GrokModelObservation {
            model_id: "grok-4.5".to_string(),
            observed_at: Some(
                DateTime::parse_from_rfc3339("2026-08-16T18:32:00Z")
                    .expect("prompt 0")
                    .with_timezone(&Utc),
            ),
        },
        GrokModelObservation {
            model_id: "grok-4.6".to_string(),
            observed_at: Some(
                DateTime::parse_from_rfc3339("2026-08-16T18:33:00Z")
                    .expect("prompt 1")
                    .with_timezone(&Utc),
            ),
        },
    ];
    let current = model_info("grok-4.6-build");

    let first_model = resolve_grok_inference_sample_model(
        &GrokInferenceSample {
            usage: UsageCounts::default(),
            observed_at: Some(first),
        },
        &prompt_models,
        &[],
        &["grok-4.5".to_string(), "grok-4.6".to_string()],
        Some(&current),
    )
    .expect("first model");
    let second_model = resolve_grok_inference_sample_model(
        &GrokInferenceSample {
            usage: UsageCounts::default(),
            observed_at: Some(second),
        },
        &prompt_models,
        &[],
        &["grok-4.5".to_string(), "grok-4.6".to_string()],
        Some(&current),
    )
    .expect("second model");
    let unresolved = resolve_grok_inference_sample_model(
        &GrokInferenceSample {
            usage: UsageCounts::default(),
            observed_at: Some(first),
        },
        &[],
        &[],
        &["grok-4.5".to_string(), "grok-4.6".to_string()],
        Some(&current),
    );

    assert_eq!(first_model.name.as_deref(), Some("grok-4.5"));
    assert_eq!(first_model.normalized_name.as_deref(), Some("grok-4.5"));
    assert_eq!(second_model.name.as_deref(), Some("grok-4.6"));
    assert_eq!(second_model.normalized_name.as_deref(), Some("grok-4.6"));
    assert_eq!(unresolved, None);
}

#[test]
fn grok_inference_model_resolution_rejects_partial_observation_when_models_used_is_mixed() {
    let observed_at = DateTime::parse_from_rfc3339("2026-08-16T18:32:10Z")
        .expect("observed")
        .with_timezone(&Utc);
    let sample = GrokInferenceSample {
        usage: UsageCounts::default(),
        observed_at: Some(observed_at),
    };
    let prompt_models = [GrokModelObservation {
        model_id: "grok-4.5".to_string(),
        observed_at: Some(
            DateTime::parse_from_rfc3339("2026-08-16T18:32:00Z")
                .expect("prompt")
                .with_timezone(&Utc),
        ),
    }];
    let current = model_info("grok-4.6-build");

    let mixed = resolve_grok_inference_sample_model(
        &sample,
        &prompt_models,
        &[],
        &["grok-4.5".to_string(), "grok-4.6".to_string()],
        Some(&current),
    );
    let matching = resolve_grok_inference_sample_model(
        &sample,
        &prompt_models,
        &[],
        &["grok-4.5".to_string()],
        Some(&current),
    )
    .expect("matching modelsUsed");
    let empty_used =
        resolve_grok_inference_sample_model(&sample, &prompt_models, &[], &[], Some(&current))
            .expect("empty modelsUsed");

    assert_eq!(mixed, None);
    assert_eq!(matching.name.as_deref(), Some("grok-4.5"));
    assert_eq!(empty_used.name.as_deref(), Some("grok-4.5"));
}
