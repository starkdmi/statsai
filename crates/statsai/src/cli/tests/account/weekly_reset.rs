use super::*;
use crate::cli::weekly_reset::{
    parse_weekly_reset_timestamp, resolve_manual_weekly_account, weekly_reset_view,
};

fn at(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, min, sec)
        .single()
        .expect("timestamp")
}

fn claude_account(store: &Store, label: &str) -> statsai_core::ProviderAccount {
    let account = test_account(
        "claude_code",
        Some(label),
        None,
        None,
        None,
        at(2026, 1, 1, 0, 0, 0),
    );
    store.upsert_account(&account).expect("account");
    account
}

#[test]
fn weekly_reset_timestamps_require_an_explicit_offset() {
    let z = parse_weekly_reset_timestamp("2026-01-08T18:00:00Z").expect("z");
    let offset = parse_weekly_reset_timestamp("2026-01-08T18:00:00+00:00").expect("offset");
    let shifted = parse_weekly_reset_timestamp("2026-01-08T13:00:00-05:00").expect("shifted");
    assert_eq!(z, offset);
    assert_eq!(z, shifted);
    assert_eq!(z, at(2026, 1, 8, 18, 0, 0));
    let fractional = parse_weekly_reset_timestamp("2026-01-08T18:00:00.900Z").expect("fraction");
    assert_eq!(fractional, at(2026, 1, 8, 18, 0, 0));
    let past = parse_weekly_reset_timestamp("2020-01-08T18:00:00Z").expect("past");
    let future = parse_weekly_reset_timestamp("2030-01-08T18:00:00Z").expect("future");
    assert!(past < future);
    let missing = parse_weekly_reset_timestamp("2026-01-08T18:00:00").expect_err("missing offset");
    assert!(missing.to_string().contains("explicit offset or Z"));
}

#[test]
fn show_prints_the_same_cycle_for_anchors_a_week_apart() {
    let store = Store::in_memory().expect("store");
    let account = claude_account(&store, "work");
    let earlier = store
        .upsert_weekly_reset_anchor(
            "claude_code",
            &account.provider_account_id,
            at(2026, 1, 8, 18, 0, 0),
        )
        .expect("earlier");
    let now = at(2026, 1, 20, 12, 0, 0);
    let first = weekly_reset_view(&account, &earlier, now);
    store
        .upsert_weekly_reset_anchor(
            "claude_code",
            &account.provider_account_id,
            at(2026, 1, 15, 18, 0, 0),
        )
        .expect("later");
    let later = store
        .weekly_reset_anchor("claude_code", &account.provider_account_id)
        .expect("read")
        .expect("anchor");
    assert_ne!(earlier.anchor, later.anchor);
    assert_eq!(weekly_reset_view(&account, &later, now), first);
    assert_eq!(first.anchor, first.current_cycle_end);
    assert_eq!(first.current_cycle_start, "2026-01-15T18:00:00Z");
    assert_eq!(first.current_cycle_end, "2026-01-22T18:00:00Z");
}

#[test]
fn weekly_reset_commands_reject_ambiguous_accounts_and_other_providers() {
    let store = Store::in_memory().expect("store");
    claude_account(&store, "work");
    let codex = test_account(
        "codex",
        Some("work"),
        None,
        None,
        None,
        at(2026, 1, 1, 0, 0, 0),
    );
    store.upsert_account(&codex).expect("codex");
    let wrong_provider = resolve_manual_weekly_account(&store, "codex", "work").expect_err("codex");
    assert!(wrong_provider
        .to_string()
        .contains("only supported for claude_code"));
    let missing =
        resolve_manual_weekly_account(&store, "claude_code", "missing").expect_err("missing");
    assert!(missing
        .to_string()
        .contains("no claude_code account matched"));
    claude_account(&store, "ada");
    let by_email = test_account(
        "claude_code",
        Some("other"),
        Some("ada"),
        None,
        None,
        at(2026, 1, 1, 0, 0, 0),
    );
    store.upsert_account(&by_email).expect("email account");
    let ambiguous =
        resolve_manual_weekly_account(&store, "claude_code", "ada").expect_err("ambiguous");
    assert!(ambiguous
        .to_string()
        .contains("multiple claude_code accounts matched"));
}

#[test]
fn merge_transfers_coalesces_and_rejects_weekly_reset_schedules() {
    let store = Store::in_memory().expect("store");
    let from = claude_account(&store, "legacy");
    let to = claude_account(&store, "canonical");
    store
        .upsert_weekly_reset_anchor(
            "claude_code",
            &from.provider_account_id,
            at(2026, 1, 8, 18, 0, 0),
        )
        .expect("from anchor");
    let transferred = merge_provider_accounts(&store, "claude_code", "legacy", "canonical", false)
        .expect("transfer");
    assert_eq!(transferred.moved_weekly_reset_schedules, 1);
    assert!(store
        .weekly_reset_anchor("claude_code", &from.provider_account_id)
        .expect("from")
        .is_none());
    assert_eq!(
        store
            .weekly_reset_anchor("claude_code", &to.provider_account_id)
            .expect("to")
            .expect("moved")
            .anchor,
        at(2026, 1, 8, 18, 0, 0)
    );

    let other = claude_account(&store, "other");
    store
        .upsert_weekly_reset_anchor(
            "claude_code",
            &other.provider_account_id,
            at(2026, 1, 15, 18, 0, 0),
        )
        .expect("equivalent");
    let coalesced = merge_provider_accounts(&store, "claude_code", "other", "canonical", false)
        .expect("coalesce");
    assert_eq!(coalesced.coalesced_weekly_reset_schedules, 1);
    assert_eq!(coalesced.moved_weekly_reset_schedules, 0);
    assert!(store
        .weekly_reset_anchor("claude_code", &to.provider_account_id)
        .expect("kept")
        .is_some());

    let mismatched = claude_account(&store, "mismatched");
    store
        .upsert_weekly_reset_anchor(
            "claude_code",
            &mismatched.provider_account_id,
            at(2026, 1, 8, 19, 0, 0),
        )
        .expect("mismatch");
    let rejected = merge_provider_accounts(&store, "claude_code", "mismatched", "canonical", false)
        .expect_err("reject");
    assert!(rejected
        .to_string()
        .contains("different weekly reset times"));
    assert!(store
        .weekly_reset_anchor("claude_code", &mismatched.provider_account_id)
        .expect("still stored")
        .is_some());
    assert!(store
        .weekly_reset_anchor("claude_code", &to.provider_account_id)
        .expect("untouched")
        .is_some());
}

#[test]
fn failed_merge_keeps_the_source_weekly_reset_anchor() {
    let store = Store::in_memory().expect("store");
    let from = claude_account(&store, "legacy");
    let to = claude_account(&store, "canonical");
    let anchor = at(2026, 1, 8, 18, 0, 0);
    let started_at = at(2026, 1, 1, 0, 0, 0);
    store
        .upsert_weekly_reset_anchor("claude_code", &from.provider_account_id, anchor)
        .expect("anchor");
    for (account_id, plan) in [
        (&from.provider_account_id, "Pro"),
        (&to.provider_account_id, "Max"),
    ] {
        store
            .upsert_subscription(&Subscription {
                schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
                subscription_id: subscription_id("claude_code", account_id, plan, started_at),
                provider: "claude_code".to_string(),
                provider_account_id: account_id.clone(),
                plan_name: plan.to_string(),
                price: 2000,
                currency: "USD".to_string(),
                billing_period: BillingPeriod::Monthly,
                paid_at: Some(started_at),
                renewal_day: Some(1),
                started_at,
                ended_at: None,
                current_period_ends_at: None,
                status: SubscriptionStatus::Active,
                record_source: IdentitySource::LocalAuth,
                verified_at: Some(started_at),
                notes: None,
            })
            .expect("subscription");
    }

    let rejected = merge_provider_accounts(&store, "claude_code", "legacy", "canonical", false)
        .expect_err("overlap");
    assert!(rejected.to_string().contains("subscription overlaps"));
    assert_eq!(
        store
            .weekly_reset_anchor("claude_code", &from.provider_account_id)
            .expect("source schedule")
            .expect("anchor restored")
            .anchor,
        anchor
    );
    assert!(store
        .weekly_reset_anchor("claude_code", &to.provider_account_id)
        .expect("destination schedule")
        .is_none());

    let equivalent = at(2026, 1, 15, 18, 0, 0);
    store
        .upsert_weekly_reset_anchor("claude_code", &to.provider_account_id, equivalent)
        .expect("destination anchor");
    let coalesced = merge_provider_accounts(&store, "claude_code", "legacy", "canonical", false)
        .expect_err("coalesce overlap");
    assert!(coalesced.to_string().contains("subscription overlaps"));
    assert_eq!(
        store
            .weekly_reset_anchor("claude_code", &from.provider_account_id)
            .expect("source schedule")
            .expect("source anchor kept")
            .anchor,
        anchor
    );
    assert_eq!(
        store
            .weekly_reset_anchor("claude_code", &to.provider_account_id)
            .expect("destination schedule")
            .expect("destination anchor kept")
            .anchor,
        equivalent
    );
}

#[test]
fn remove_treats_a_weekly_reset_schedule_as_a_reference() {
    let store = Store::in_memory().expect("store");
    let account = claude_account(&store, "work");
    store
        .upsert_weekly_reset_anchor(
            "claude_code",
            &account.provider_account_id,
            at(2026, 1, 8, 18, 0, 0),
        )
        .expect("anchor");
    let blocked =
        remove_orphan_provider_account(&store, "claude_code", "work", false).expect_err("blocked");
    assert!(blocked.to_string().contains("weekly reset schedules"));
    assert!(store
        .delete_weekly_reset_anchor("claude_code", &account.provider_account_id)
        .expect("clear"));
    let removed =
        remove_orphan_provider_account(&store, "claude_code", "work", false).expect("remove");
    assert!(removed.deleted);
}

#[test]
fn anchor_changes_retire_manual_contribution_ids_on_the_next_sync() {
    let store = Store::in_memory().expect("store");
    let account = claude_account(&store, "work");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/statsai-weekly-reset"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    store
        .upsert_weekly_reset_anchor(
            "claude_code",
            &account.provider_account_id,
            at(2026, 1, 8, 18, 0, 0),
        )
        .expect("anchor");
    let mut event = test_event(
        "claude_code",
        &source,
        at(2026, 1, 10, 12, 0, 0),
        Some(account.provider_account_id.clone()),
        TokenParts::total(12),
    );
    event.provider = "claude_code".to_string();
    store.insert_event(&event).expect("event");

    let command = test_sync_command("http");
    let target = sync_target(&command).expect("target");
    let (first, _) = build_sync_batch(&command, &store, "device-weekly", &target).expect("first");
    let original_ids = first
        .quota_cycle_contributions
        .iter()
        .map(|contribution| contribution.contribution_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(original_ids.len(), 1);
    assert!(first
        .authoritative_snapshot
        .as_ref()
        .expect("first snapshot")
        .quota_cycle_contribution_ids
        .iter()
        .any(|id| id == &original_ids[0]));
    record_rollup_sync_success(&store, "http", &target, &first).expect("record");

    store
        .upsert_weekly_reset_anchor(
            "claude_code",
            &account.provider_account_id,
            at(2026, 1, 8, 21, 0, 0),
        )
        .expect("shift");
    let (shifted, _) =
        build_sync_batch(&command, &store, "device-weekly", &target).expect("shifted");
    let snapshot = shifted
        .authoritative_snapshot
        .as_ref()
        .expect("retirement snapshot");
    assert!(snapshot
        .quota_cycle_contribution_ids
        .iter()
        .all(|id| id != &original_ids[0]));
    assert!(!shifted.quota_cycle_contributions.is_empty());
    record_rollup_sync_success(&store, "http", &target, &shifted).expect("record shift");

    assert!(store
        .delete_weekly_reset_anchor("claude_code", &account.provider_account_id)
        .expect("clear"));
    let (cleared, _) =
        build_sync_batch(&command, &store, "device-weekly", &target).expect("cleared");
    let cleared_snapshot = cleared.authoritative_snapshot.expect("clear snapshot");
    assert!(cleared_snapshot.quota_cycle_contribution_ids.is_empty());
    assert!(cleared.quota_cycle_contributions.is_empty());
}
