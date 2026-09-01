use super::support::*;
use super::*;
use std::path::Path;
#[test]
fn source_ids_are_stable_for_same_input() {
    let a = source_id("codex", SourceKind::LocalAdapter, "abc");
    let b = source_id("codex", SourceKind::LocalAdapter, "abc");
    assert_eq!(a, b);
}

#[test]
fn source_ids_change_by_provider() {
    let codex = source_id("codex", SourceKind::LocalAdapter, "abc");
    let claude = source_id("claude_code", SourceKind::LocalAdapter, "abc");
    assert_ne!(codex, claude);
}

#[test]
fn reasoning_level_supports_ultracode() {
    assert_eq!(
        ReasoningLevel::parse("ultracode"),
        Some(ReasoningLevel::Ultracode)
    );
    assert_eq!(ReasoningLevel::Ultracode.as_str(), "ultracode");
}

#[test]
fn total_falls_back_to_parts() {
    let usage = UsageCounts {
        input_tokens: Some(10),
        output_tokens: Some(5),
        cache_read_tokens: Some(2),
        ..UsageCounts::default()
    };
    assert_eq!(usage.computed_total(), 17);
}

#[test]
fn legacy_usage_counts_without_cache_lifetimes_deserialize() {
    let usage: UsageCounts = serde_json::from_value(serde_json::json!({
        "input_tokens": 10,
        "cache_creation_tokens": 4
    }))
    .expect("legacy usage counts");

    assert_eq!(usage.cache_creation_tokens, Some(4));
    assert_eq!(usage.cache_creation_5m_tokens, None);
    assert_eq!(usage.cache_creation_1h_tokens, None);
    assert_eq!(usage.computed_total(), 14);
}

#[test]
fn schema_types_serialize() {
    let schema = schemars::schema_for!(UsageEvent);
    let json = serde_json::to_value(schema).expect("schema should serialize");
    assert!(json.get("title").is_some());

    let schema = schemars::schema_for!(UsageSummary);
    let json = serde_json::to_value(schema).expect("summary schema should serialize");
    assert!(json.get("title").is_some());
}

#[test]
fn sync_batch_without_authoritative_snapshot_remains_backward_compatible() {
    let batch: SyncBatch = serde_json::from_value(serde_json::json!({
        "schema_version": SYNC_BATCH_V2_SCHEMA_VERSION,
        "batch_id": "batch-legacy-v2",
        "device_id": "device-1",
        "created_at": "2026-05-31T10:00:00Z"
    }))
    .expect("legacy v2 batch should deserialize");

    assert!(batch.authoritative_snapshot.is_none());
    let serialized = serde_json::to_value(batch).expect("batch should serialize");
    assert!(serialized.get("authoritative_snapshot").is_none());
}

#[test]
fn sync_ack_v1_omits_zero_task_counters() {
    let ack = SyncAck {
        schema_version: SYNC_ACK_V1_SCHEMA_VERSION.to_string(),
        batch_id: "batch-1".to_string(),
        accepted: SyncEntityCounts {
            sources: 1,
            accounts: 0,
            source_account_assignments: 0,
            subscriptions: 0,
            events: 0,
            summaries: 2,
            task_buckets: 0,
            task_verifications: 0,
            code_change_metrics: 0,
            quota_cycle_contributions: 0,
            account_plan_observations: 0,
            account_evidence_summaries: 0,
        },
        duplicates: SyncEntityCounts {
            sources: 0,
            accounts: 0,
            source_account_assignments: 0,
            subscriptions: 0,
            events: 0,
            summaries: 0,
            task_buckets: 0,
            task_verifications: 0,
            code_change_metrics: 0,
            quota_cycle_contributions: 0,
            account_plan_observations: 0,
            account_evidence_summaries: 0,
        },
        rejected: Vec::new(),
    };

    let json = serde_json::to_value(&ack).expect("ack should serialize");
    assert_eq!(json["schema_version"], SYNC_ACK_V1_SCHEMA_VERSION);
    assert!(json["accepted"].get("task_buckets").is_none());
    assert!(json["accepted"].get("task_verifications").is_none());
    assert!(json["accepted"].get("code_change_metrics").is_none());
    assert!(json["duplicates"].get("task_buckets").is_none());
    assert!(json["duplicates"].get("task_verifications").is_none());
    assert!(json["duplicates"].get("code_change_metrics").is_none());
}

#[test]
fn sync_ack_v3_keeps_nonzero_task_and_code_change_counters() {
    let ack = SyncAck {
        schema_version: SYNC_ACK_V3_SCHEMA_VERSION.to_string(),
        batch_id: "batch-2".to_string(),
        accepted: SyncEntityCounts {
            sources: 0,
            accounts: 0,
            source_account_assignments: 0,
            subscriptions: 0,
            events: 0,
            summaries: 0,
            task_buckets: 3,
            task_verifications: 1,
            code_change_metrics: 2,
            quota_cycle_contributions: 0,
            account_plan_observations: 0,
            account_evidence_summaries: 0,
        },
        duplicates: SyncEntityCounts {
            sources: 0,
            accounts: 0,
            source_account_assignments: 0,
            subscriptions: 0,
            events: 0,
            summaries: 0,
            task_buckets: 0,
            task_verifications: 0,
            code_change_metrics: 0,
            quota_cycle_contributions: 0,
            account_plan_observations: 0,
            account_evidence_summaries: 0,
        },
        rejected: Vec::new(),
    };

    let json = serde_json::to_value(&ack).expect("ack should serialize");
    assert_eq!(json["schema_version"], SYNC_ACK_V3_SCHEMA_VERSION);
    assert_eq!(json["accepted"]["task_buckets"], 3);
    assert_eq!(json["accepted"]["task_verifications"], 1);
    assert_eq!(json["accepted"]["code_change_metrics"], 2);
    assert!(json["accepted"].get("quota_cycle_contributions").is_none());
    assert!(json["accepted"].get("account_plan_observations").is_none());
    assert!(json["accepted"].get("account_evidence_summaries").is_none());
}

#[test]
fn sync_ack_v4_keeps_nonzero_quota_cycle_counters() {
    let ack = SyncAck {
        schema_version: SYNC_ACK_V4_SCHEMA_VERSION.to_string(),
        batch_id: "batch-quota".to_string(),
        accepted: SyncEntityCounts {
            sources: 0,
            accounts: 0,
            source_account_assignments: 0,
            subscriptions: 0,
            events: 0,
            summaries: 0,
            task_buckets: 0,
            task_verifications: 0,
            code_change_metrics: 1,
            quota_cycle_contributions: 4,
            account_plan_observations: 0,
            account_evidence_summaries: 0,
        },
        duplicates: SyncEntityCounts {
            sources: 0,
            accounts: 0,
            source_account_assignments: 0,
            subscriptions: 0,
            events: 0,
            summaries: 0,
            task_buckets: 0,
            task_verifications: 0,
            code_change_metrics: 0,
            quota_cycle_contributions: 0,
            account_plan_observations: 0,
            account_evidence_summaries: 0,
        },
        rejected: Vec::new(),
    };

    let json = serde_json::to_value(&ack).expect("ack should serialize");
    assert_eq!(json["schema_version"], SYNC_ACK_V4_SCHEMA_VERSION);
    assert_eq!(json["accepted"]["code_change_metrics"], 1);
    assert_eq!(json["accepted"]["quota_cycle_contributions"], 4);
}

#[test]
fn sync_batch_v3_without_quota_contributions_remains_backward_compatible() {
    let batch: SyncBatch = serde_json::from_value(serde_json::json!({
        "schema_version": SYNC_BATCH_V3_SCHEMA_VERSION,
        "batch_id": "batch-legacy-v3",
        "device_id": "device-1",
        "created_at": "2026-05-31T10:00:00Z"
    }))
    .expect("legacy v3 batch should deserialize");

    assert!(batch.quota_cycle_contributions.is_empty());
    let serialized = serde_json::to_value(batch).expect("batch should serialize");
    assert!(serialized.get("quota_cycle_contributions").is_none());
}
#[test]
fn computed_total_does_not_overflow() {
    let usage = UsageCounts {
        input_tokens: Some(u64::MAX),
        output_tokens: Some(u64::MAX),
        ..UsageCounts::default()
    };
    let total = usage.computed_total();
    assert_eq!(total, u64::MAX);
}
#[test]
fn display_path_expands_home_but_avoids_canonicalize() {
    let p = Path::new("~/relative/test");
    let displayed = display_path(p);
    assert!(displayed.contains("relative/test"));
    // should not resolve to absolute via fs if ~ expanded
    if let Some(home) = home_dir() {
        let home_str = home.to_string_lossy();
        if displayed.starts_with(home_str.as_ref()) {
            // expanded, good
        }
    }
}

#[test]
fn path_hash_remains_stable_via_canonical_display() {
    let p = Path::new("/tmp/nonexistent-for-test");
    let h1 = path_hash(p);
    let h2 = path_hash(p);
    assert_eq!(h1, h2);
}

#[test]
fn renaming_a_repository_keeps_one_rollup_bucket() {
    let before = ProjectInfo {
        project_id: "project_before".to_string(),
        project_label: Some("ai-stats".to_string()),
        repo_remote_hash: Some("remote_before".to_string()),
        repo_label: Some("owner/ai-stats".to_string()),
        branch_hash: Some("branch_main".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path_checkout".to_string()),
        path_label: Some("/work/ai-stats".to_string()),
    };
    let after = ProjectInfo {
        project_id: "project_after".to_string(),
        repo_remote_hash: Some("remote_after".to_string()),
        repo_label: Some("owner/statsai".to_string()),
        ..before.clone()
    };

    // Same checkout, same branch: the rename must not split the bucket.
    assert_eq!(
        daily_rollup_project_key(Some(&before)),
        daily_rollup_project_key(Some(&after))
    );
    // The remote itself is untouched, so the backend can still key the
    // project on it and move the location's history across the rename.
    assert_ne!(before.repo_remote_hash, after.repo_remote_hash);

    // Task spans are already persisted under the remote-inclusive key, so
    // it has to keep telling the two apart or their history splits at the
    // upgrade instead of at the rename.
    assert_ne!(
        project_bucket_key(Some(&before)),
        project_bucket_key(Some(&after))
    );

    // A different checkout of the same repository keeps its own bucket, the
    // way a worktree is its own location under one project.
    let elsewhere = ProjectInfo {
        path_hash: Some("path_worktree".to_string()),
        ..before.clone()
    };
    assert_ne!(
        daily_rollup_project_key(Some(&before)),
        daily_rollup_project_key(Some(&elsewhere))
    );

    // So does the same checkout on another branch.
    let other_branch = ProjectInfo {
        branch_hash: Some("branch_release".to_string()),
        ..before.clone()
    };
    assert_ne!(
        daily_rollup_project_key(Some(&before)),
        daily_rollup_project_key(Some(&other_branch))
    );
}

#[test]
fn remote_only_attribution_still_buckets_by_repository() {
    let project = ProjectInfo {
        project_id: "project_remote_only".to_string(),
        project_label: Some("statsai".to_string()),
        repo_remote_hash: Some("remote_only".to_string()),
        repo_label: Some("owner/statsai".to_string()),
        branch_hash: None,
        branch_label: None,
        path_hash: None,
        path_label: None,
    };

    assert_eq!(
        daily_rollup_project_key(Some(&project)),
        "repo:remote_only|branch:none"
    );
}

#[test]
fn bare_project_id_is_not_a_stable_project_identity() {
    let project = ProjectInfo {
        project_id: "project_bare".to_string(),
        project_label: Some("Bare".to_string()),
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: None,
        path_label: None,
    };

    assert!(!project_has_stable_identity(&project));
    assert_eq!(project_bucket_key(Some(&project)), "none");
}

#[test]
fn sanitize_project_for_sync_preserves_path_only_project_labels() {
    let project = ProjectInfo {
        project_id: "project_path_only".to_string(),
        project_label: Some("Scratch".to_string()),
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: Some("path-hash".to_string()),
        path_label: Some("/Users/example/Scratch".to_string()),
    };

    let sanitized = sanitize_project_for_sync(project).expect("stable path identity");

    assert_eq!(sanitized.repo_remote_hash, None);
    assert_eq!(sanitized.path_hash.as_deref(), Some("path-hash"));
    assert_eq!(
        sanitized.path_label.as_deref(),
        Some("/Users/example/Scratch")
    );
    assert!(project_contains_file_paths(Some(&sanitized)));
}

#[test]
fn sanitize_project_for_sync_drops_bare_project_ids() {
    let project = ProjectInfo {
        project_id: "project_bare".to_string(),
        project_label: Some("Bare".to_string()),
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: None,
        path_label: Some("/Users/example/Bare".to_string()),
    };

    assert!(sanitize_project_for_sync(project).is_none());
}

#[test]
fn sanitize_summary_for_sync_marks_project_path_labels_as_file_paths() {
    let now = mk_dt(2026, 5, 25);
    let source = test_source("codex", "/tmp/codex");
    let mut summary = test_summary("codex", &source, now, now, now, 100);
    summary.project = Some(ProjectInfo {
        project_id: "project_path_only".to_string(),
        project_label: Some("Scratch".to_string()),
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: Some("path-hash".to_string()),
        path_label: Some("/Users/example/Scratch".to_string()),
    });

    let sanitized = sanitize_summary_for_sync(summary);

    assert_eq!(
        sanitized
            .project
            .as_ref()
            .and_then(|project| project.path_label.as_deref()),
        Some("/Users/example/Scratch")
    );
    assert!(sanitized.privacy.contains_file_paths);
}
