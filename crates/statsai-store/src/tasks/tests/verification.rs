use super::support::*;
use super::*;

#[test]
fn resolve_task_verifications_keeps_latest_status_and_rename_per_anchor() {
    let created_at = Utc.with_ymd_and_hms(2026, 7, 1, 10, 0, 0).unwrap();
    let anchor_span_id = TaskSpanId("span-anchor".to_string());
    let work_item_id = WorkItemId("work-anchor".to_string());
    let reject = TaskVerification {
        schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
        verification_id: task_verification_id("reject", "status:span-anchor"),
        action_key: "status:span-anchor".to_string(),
        action: TaskVerificationAction::Reject {
            work_item_id: work_item_id.clone(),
            anchor_span_id: anchor_span_id.clone(),
            reason: TaskVerdict::Meta,
        },
        created_at,
        updated_at: created_at,
    };
    let rename = TaskVerification {
        schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
        verification_id: task_verification_id("rename", "rename:span-anchor"),
        action_key: "rename:span-anchor".to_string(),
        action: TaskVerificationAction::Rename {
            work_item_id,
            anchor_span_id,
            title: "Verified renamed task".to_string(),
        },
        created_at,
        updated_at: created_at + chrono::Duration::minutes(5),
    };

    let resolved = resolve_task_verifications(vec![reject, rename]);
    assert_eq!(resolved.len(), 2);
    assert!(matches!(
        resolved[0].action,
        TaskVerificationAction::Reject { .. }
    ));
    assert!(matches!(
        resolved[1].action,
        TaskVerificationAction::Rename { .. }
    ));
}

#[test]
fn merge_task_verification_canonicalizes_legacy_anchor_keys_before_insert() {
    let store = Store::in_memory().expect("store");
    let created_at = Utc.with_ymd_and_hms(2026, 7, 1, 10, 0, 0).unwrap();
    let anchor_span_id = TaskSpanId("span-anchor".to_string());
    let work_item_id = WorkItemId("work-anchor".to_string());
    let legacy_rename = TaskVerification {
        schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
        verification_id: TaskVerificationId("legacy-rename".to_string()),
        action_key: "anchor:span-anchor".to_string(),
        action: TaskVerificationAction::Rename {
            work_item_id: work_item_id.clone(),
            anchor_span_id: anchor_span_id.clone(),
            title: "Legacy rename".to_string(),
        },
        created_at,
        updated_at: created_at,
    };
    let payload = serde_json::to_string(&legacy_rename).expect("legacy payload");
    store
        .conn
        .execute(
            r#"
                INSERT INTO task_verifications (
                  verification_id, action_kind, action_key, updated_at, payload
                )
                VALUES (?1, ?2, ?3, ?4, ?5)
                "#,
            rusqlite::params![
                &legacy_rename.verification_id.0,
                legacy_rename.action.action_kind(),
                &legacy_rename.action_key,
                legacy_rename.updated_at.to_rfc3339(),
                &payload,
            ],
        )
        .expect("insert legacy rename");

    let legacy_reject = TaskVerification {
        schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
        verification_id: TaskVerificationId("legacy-reject".to_string()),
        action_key: "anchor:span-anchor".to_string(),
        action: TaskVerificationAction::Reject {
            work_item_id,
            anchor_span_id,
            reason: TaskVerdict::Meta,
        },
        created_at: created_at + chrono::Duration::minutes(1),
        updated_at: created_at + chrono::Duration::minutes(1),
    };

    assert!(store
        .merge_task_verification(&legacy_reject)
        .expect("merge legacy reject"));

    let stored = store.task_verifications().expect("task verifications");
    assert_eq!(stored.len(), 2);
    assert!(stored.iter().any(|verification| {
        matches!(verification.action, TaskVerificationAction::Rename { .. })
            && verification.action_key == "anchor:span-anchor"
    }));
    assert!(stored.iter().any(|verification| {
        matches!(verification.action, TaskVerificationAction::Reject { .. })
            && verification.action_key == "status:span-anchor"
    }));
}
