use super::support::*;
use super::*;
use chrono::TimeZone;

fn at(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, min, sec)
        .single()
        .expect("timestamp")
}

fn anchor_account(store: &Store, anchor: DateTime<Utc>) -> ProviderAccountId {
    let account_id = ProviderAccountId("account-claude".to_string());
    store
        .upsert_weekly_reset_anchor("claude_code", &account_id, anchor)
        .expect("anchor");
    account_id
}

fn claude_event(
    store: &Store,
    account_id: &ProviderAccountId,
    started_at: DateTime<Utc>,
    record_id: &str,
    usage: UsageCounts,
    estimated_cost_micro_usd: i64,
) {
    let (source_id, _) = assigned_source(store, at(2024, 1, 1, 0, 0, 0));
    let mut event = sample_usage_event(
        &source_id,
        account_id,
        started_at,
        record_id,
        usage.input_tokens.unwrap_or(0),
        usage.cache_read_tokens.unwrap_or(0),
        usage.output_tokens.unwrap_or(0),
        usage.reasoning_tokens.unwrap_or(0),
        estimated_cost_micro_usd,
    );
    event.provider = "claude_code".to_string();
    event.usage = usage;
    event.event_id = event_id("claude_code", &source_id, record_id, None, started_at);
    store.insert_event(&event).expect("event");
}

fn simple_usage(total: u64) -> UsageCounts {
    UsageCounts {
        input_tokens: Some(total),
        total_tokens: Some(total),
        ..UsageCounts::default()
    }
}

fn contributions_at(
    store: &Store,
    now: DateTime<Utc>,
) -> Vec<statsai_core::QuotaCycleContributionV1> {
    store
        .manual_weekly_contributions(&QuotaQuery::default(), "device-weekly", now)
        .expect("contributions")
}

#[test]
fn weekly_cycle_containing_is_half_open_and_week_equivalent() {
    let anchor = at(2026, 1, 8, 18, 0, 0);
    let later = anchor + Duration::seconds(WEEKLY_RESET_PERIOD_SECONDS);
    let now = at(2026, 1, 20, 12, 0, 0);
    assert_eq!(
        weekly_cycle_containing(anchor, now),
        weekly_cycle_containing(later, now)
    );
    let (start, end) = weekly_cycle_containing(anchor, now);
    assert_eq!(end - start, Duration::seconds(WEEKLY_RESET_PERIOD_SECONDS));
    assert!(start <= now && now < end);
    assert_eq!(
        end.timestamp().rem_euclid(WEEKLY_RESET_PERIOD_SECONDS),
        anchor.timestamp().rem_euclid(WEEKLY_RESET_PERIOD_SECONDS)
    );
    let (reset_start, reset_end) = weekly_cycle_containing(anchor, anchor);
    assert_eq!(reset_start, anchor);
    assert_eq!(
        reset_end,
        anchor + Duration::seconds(WEEKLY_RESET_PERIOD_SECONDS)
    );
}

#[test]
fn manual_weekly_anchor_rejects_other_providers_and_stores_whole_seconds() {
    let store = Store::in_memory().expect("store");
    let account_id = ProviderAccountId("account-claude".to_string());
    let error = store
        .upsert_weekly_reset_anchor("codex", &account_id, at(2026, 1, 8, 18, 0, 0))
        .expect_err("codex anchor");
    assert!(error.to_string().contains("only supported for claude_code"));
    let fractional = at(2026, 1, 8, 18, 0, 0) + Duration::nanoseconds(900_000_000);
    store
        .upsert_weekly_reset_anchor("claude_code", &account_id, fractional)
        .expect("anchor");
    let stored = store
        .weekly_reset_anchor("claude_code", &account_id)
        .expect("read")
        .expect("stored");
    assert_eq!(stored.anchor, at(2026, 1, 8, 18, 0, 0));
    assert_eq!(stored.source, "manual");
    let created_at = stored.created_at;
    store
        .upsert_weekly_reset_anchor("claude_code", &account_id, at(2026, 3, 1, 4, 0, 0))
        .expect("replace");
    let replaced = store
        .weekly_reset_anchor("claude_code", &account_id)
        .expect("read")
        .expect("stored");
    assert_eq!(replaced.anchor, at(2026, 3, 1, 4, 0, 0));
    assert_eq!(replaced.created_at, created_at);
    assert!(replaced.updated_at >= created_at);
}

#[test]
fn equivalent_anchors_emit_the_same_completed_weeks() {
    let now = at(2026, 2, 1, 0, 0, 0);
    let event_at = at(2026, 1, 10, 12, 0, 0);
    let mut ids = Vec::new();
    for anchor in [
        at(2026, 1, 8, 18, 0, 0),
        at(2026, 1, 8, 18, 0, 0) + Duration::seconds(WEEKLY_RESET_PERIOD_SECONDS),
    ] {
        let store = Store::in_memory().expect("store");
        let account_id = anchor_account(&store, anchor);
        claude_event(
            &store,
            &account_id,
            event_at,
            "inside",
            simple_usage(10),
            10,
        );
        let contributions = contributions_at(&store, now);
        assert_eq!(contributions.len(), 1);
        ids.push(contributions[0].contribution_id.clone());
        assert_eq!(
            contributions[0].representative_reset,
            at(2026, 1, 15, 18, 0, 0)
        );
    }
    assert_eq!(ids[0], ids[1]);
}

#[test]
fn emission_rules_skip_empty_weeks_and_the_current_cycle() {
    let store = Store::in_memory().expect("store");
    let account_id = anchor_account(&store, at(2026, 1, 8, 18, 0, 0));
    claude_event(
        &store,
        &account_id,
        at(2026, 1, 7, 12, 0, 0),
        "early",
        simple_usage(10),
        10,
    );
    claude_event(
        &store,
        &account_id,
        at(2026, 1, 20, 12, 0, 0),
        "later",
        simple_usage(10),
        10,
    );
    let now = at(2026, 1, 30, 0, 0, 0);
    let ends = contributions_at(&store, now)
        .into_iter()
        .map(|contribution| contribution.representative_reset)
        .collect::<Vec<_>>();
    assert_eq!(
        ends,
        vec![at(2026, 1, 8, 18, 0, 0), at(2026, 1, 22, 18, 0, 0)]
    );

    let during_current = contributions_at(&store, at(2026, 1, 21, 0, 0, 0));
    assert_eq!(during_current.len(), 1);
    assert_eq!(
        during_current[0].representative_reset,
        at(2026, 1, 8, 18, 0, 0)
    );
}

#[test]
fn a_reset_day_before_the_reset_emits_the_following_week_with_a_zero_slice() {
    let store = Store::in_memory().expect("store");
    let account_id = anchor_account(&store, at(2026, 1, 8, 18, 0, 0));
    claude_event(
        &store,
        &account_id,
        at(2026, 1, 15, 10, 0, 0),
        "before-reset",
        simple_usage(70),
        700,
    );
    let contributions = contributions_at(&store, at(2026, 1, 23, 0, 0, 0));
    assert_eq!(contributions.len(), 2);
    let previous = &contributions[0];
    let following = &contributions[1];
    assert_eq!(previous.representative_reset, at(2026, 1, 15, 18, 0, 0));
    assert_eq!(following.representative_reset, at(2026, 1, 22, 18, 0, 0));
    assert_eq!(previous.boundary_slices.len(), 2);
    assert_eq!(
        previous.boundary_slices[1].period_start,
        at(2026, 1, 15, 0, 0, 0)
    );
    assert_eq!(
        previous.boundary_slices[1].period_end,
        at(2026, 1, 15, 18, 0, 0)
    );
    assert_eq!(previous.boundary_slices[1].input_tokens, 70);
    assert_eq!(
        following.boundary_slices[0].period_start,
        at(2026, 1, 15, 18, 0, 0)
    );
    assert_eq!(
        following.boundary_slices[0].period_end,
        at(2026, 1, 16, 0, 0, 0)
    );
    assert_eq!(following.boundary_slices[0].total_tokens, 0);
    assert_eq!(following.boundary_slices[0].estimated_cost_micro_usd, 0);
}

#[test]
fn an_event_exactly_at_a_reset_belongs_to_the_next_week() {
    let store = Store::in_memory().expect("store");
    let account_id = anchor_account(&store, at(2026, 1, 8, 18, 0, 0));
    claude_event(
        &store,
        &account_id,
        at(2026, 1, 15, 18, 0, 0),
        "on-reset",
        simple_usage(9),
        90,
    );
    let contributions = contributions_at(&store, at(2026, 1, 23, 0, 0, 0));
    // The instant is on the previous cycle's end day, so that week is emitted
    // too, but the event itself starts the next week.
    assert_eq!(contributions.len(), 2);
    assert_eq!(
        contributions[0].representative_reset,
        at(2026, 1, 15, 18, 0, 0)
    );
    assert_eq!(contributions[0].boundary_slices[1].input_tokens, 0);
    assert_eq!(
        contributions[1].representative_reset,
        at(2026, 1, 22, 18, 0, 0)
    );
    assert_eq!(contributions[1].boundary_slices[0].input_tokens, 9);
    assert_eq!(
        contributions[1].boundary_slices[0].period_start,
        at(2026, 1, 15, 18, 0, 0)
    );
}

#[test]
fn midnight_resets_have_no_boundary_slices_and_midday_resets_do_not_prorate() {
    let midnight = Store::in_memory().expect("store");
    let midnight_account = anchor_account(&midnight, at(2026, 1, 12, 0, 0, 0));
    claude_event(
        &midnight,
        &midnight_account,
        at(2026, 1, 7, 12, 0, 0),
        "interior",
        simple_usage(1000),
        1000,
    );
    let midnight_contributions = contributions_at(&midnight, at(2026, 1, 20, 0, 0, 0));
    assert_eq!(midnight_contributions.len(), 1);
    assert!(midnight_contributions[0].boundary_slices.is_empty());
    let containing = &midnight_contributions[0];
    assert_eq!(containing.representative_reset, at(2026, 1, 12, 0, 0, 0));
    assert!(containing.daily_envelopes.is_empty());
    assert_eq!(containing.limit_id, None);
    assert!(!containing.has_schedule_overlap);
    assert_eq!(containing.window_minutes, 10_080);

    let midday = Store::in_memory().expect("store");
    let account_id = anchor_account(&midday, at(2026, 1, 8, 18, 0, 0));
    claude_event(
        &midday,
        &account_id,
        at(2026, 1, 10, 12, 0, 0),
        "interior",
        simple_usage(1000),
        1000,
    );
    claude_event(
        &midday,
        &account_id,
        at(2026, 1, 8, 20, 0, 0),
        "boundary",
        simple_usage(40),
        40,
    );
    let contributions = contributions_at(&midday, at(2026, 1, 20, 0, 0, 0));
    let week = contributions
        .iter()
        .find(|contribution| contribution.representative_reset == at(2026, 1, 15, 18, 0, 0))
        .expect("week");
    assert_eq!(week.boundary_slices.len(), 2);
    assert_eq!(week.boundary_slices[0].input_tokens, 40);
    assert_eq!(week.boundary_slices[1].input_tokens, 0);
    assert_eq!(week.boundary_slices[0].total_tokens, 40);
    assert!(week
        .boundary_slices
        .iter()
        .all(|slice| slice.total_tokens < 1000));
}

#[test]
fn boundary_slices_keep_token_categories_and_micro_usd() {
    let store = Store::in_memory().expect("store");
    let account_id = anchor_account(&store, at(2026, 1, 8, 18, 0, 0));
    claude_event(
        &store,
        &account_id,
        at(2026, 1, 8, 20, 0, 0),
        "categories",
        UsageCounts {
            input_tokens: Some(11),
            output_tokens: Some(44),
            cache_creation_tokens: Some(22),
            cache_creation_5m_tokens: Some(22),
            cache_read_tokens: Some(33),
            reasoning_tokens: Some(55),
            total_tokens: Some(165),
            ..UsageCounts::default()
        },
        123_456,
    );
    let contributions = contributions_at(&store, at(2026, 1, 20, 0, 0, 0));
    let week = contributions
        .iter()
        .find(|contribution| contribution.representative_reset == at(2026, 1, 15, 18, 0, 0))
        .expect("week");
    let slice = &week.boundary_slices[0];
    assert_eq!(slice.input_tokens, 11);
    assert_eq!(slice.cache_creation_tokens, 22);
    assert_eq!(slice.cache_read_tokens, 33);
    assert_eq!(slice.output_tokens, 44);
    assert_eq!(slice.reasoning_tokens, 55);
    assert_eq!(slice.total_tokens, 165);
    assert_eq!(slice.estimated_cost_micro_usd, 123_456);
    assert!(week.contribution_id.starts_with("quota_cycle_"));
    assert_eq!(week.contribution_id.len(), "quota_cycle_".len() + 32);
    assert!(week
        .contribution_id
        .chars()
        .skip("quota_cycle_".len())
        .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()));
}

#[test]
fn late_imports_and_corrections_change_only_the_affected_contribution() {
    let store = Store::in_memory().expect("store");
    let account_id = anchor_account(&store, at(2026, 1, 8, 18, 0, 0));
    let now = at(2026, 2, 1, 0, 0, 0);
    claude_event(
        &store,
        &account_id,
        at(2026, 1, 10, 12, 0, 0),
        "first",
        simple_usage(10),
        10,
    );
    let before = contributions_at(&store, now);
    assert_eq!(before.len(), 1);
    let before_id = before[0].contribution_id.clone();
    let before_json = serde_json::to_string(&before[0]).expect("json");

    claude_event(
        &store,
        &account_id,
        at(2026, 1, 3, 12, 0, 0),
        "imported",
        simple_usage(5),
        5,
    );
    let after_import = contributions_at(&store, now);
    assert_eq!(after_import.len(), 2);
    let unchanged = after_import
        .iter()
        .find(|contribution| contribution.contribution_id == before_id)
        .expect("original week");
    assert_eq!(serde_json::to_string(unchanged).expect("json"), before_json);

    claude_event(
        &store,
        &account_id,
        at(2026, 1, 8, 20, 0, 0),
        "boundary-late",
        simple_usage(8),
        8,
    );
    let corrected = contributions_at(&store, now);
    let changed = corrected
        .iter()
        .find(|contribution| contribution.contribution_id == before_id)
        .expect("original week");
    assert_ne!(serde_json::to_string(changed).expect("json"), before_json);
    assert_eq!(changed.boundary_slices[0].input_tokens, 8);
    let other = corrected
        .iter()
        .find(|contribution| contribution.representative_reset == at(2026, 1, 8, 18, 0, 0))
        .expect("imported week");
    assert_eq!(other.boundary_slices.len(), 2);
    assert_eq!(other.boundary_slices[0].input_tokens, 0);
    assert_eq!(other.boundary_slices[1].input_tokens, 0);
}

#[test]
fn changing_or_clearing_the_anchor_drops_the_old_contribution_ids() {
    let store = Store::in_memory().expect("store");
    let account_id = ProviderAccountId("account-claude".to_string());
    store
        .upsert_weekly_reset_anchor("claude_code", &account_id, at(2026, 1, 8, 18, 0, 0))
        .expect("anchor");
    claude_event(
        &store,
        &account_id,
        at(2026, 1, 10, 12, 0, 0),
        "used",
        simple_usage(10),
        10,
    );
    let now = at(2026, 2, 1, 0, 0, 0);
    let original = contributions_at(&store, now);
    assert_eq!(original.len(), 1);
    store
        .upsert_weekly_reset_anchor("claude_code", &account_id, at(2026, 1, 8, 21, 0, 0))
        .expect("shift");
    let shifted = contributions_at(&store, now);
    assert!(!shifted.is_empty());
    assert!(shifted
        .iter()
        .all(|contribution| contribution.contribution_id != original[0].contribution_id));
    assert!(store
        .delete_weekly_reset_anchor("claude_code", &account_id)
        .expect("clear"));
    assert!(contributions_at(&store, now).is_empty());
    assert_eq!(
        store
            .weekly_reset_anchor_count(Some("claude_code"), &account_id)
            .expect("count"),
        0
    );
}

#[test]
fn usage_day_reads_use_the_provider_account_index() {
    let store = Store::in_memory().expect("store");
    let plan = query_plan(
        &store,
        "SELECT DISTINCT substr(started_at, 1, 10)
         FROM usage_events
         WHERE provider = 'claude_code' AND provider_account_id = ?1",
    );
    assert!(
        plan.contains("usage_events_provider_account_started_idx"),
        "usage day lookup does not use the account index: {plan}"
    );
    let bounded = query_plan(
        &store,
        "SELECT payload FROM usage_events
         WHERE provider = 'claude_code'
           AND provider_account_id = ?1
           AND (started_at >= ?2 AND started_at < ?3)",
    );
    assert!(
        bounded.contains("usage_events_provider_account_started_idx"),
        "boundary day lookup does not use the account index: {bounded}"
    );
}

#[test]
fn other_providers_and_unattributed_events_do_not_emit_a_week() {
    let store = Store::in_memory().expect("store");
    let account_id = anchor_account(&store, at(2026, 1, 8, 18, 0, 0));
    let (source_id, _) = assigned_source(&store, at(2024, 1, 1, 0, 0, 0));
    let mut codex = sample_usage_event(
        &source_id,
        &account_id,
        at(2026, 1, 10, 12, 0, 0),
        "codex-event",
        50,
        0,
        0,
        0,
        50,
    );
    codex.provider_account_id = Some(account_id.clone());
    store.insert_event(&codex).expect("codex event");
    let mut unattributed = sample_usage_event(
        &source_id,
        &account_id,
        at(2026, 1, 10, 13, 0, 0),
        "unattributed",
        50,
        0,
        0,
        0,
        50,
    );
    unattributed.provider = "claude_code".to_string();
    unattributed.provider_account_id = None;
    store.insert_event(&unattributed).expect("unattributed");
    assert!(contributions_at(&store, at(2026, 2, 1, 0, 0, 0)).is_empty());
}

#[test]
fn quota_cycle_contributions_appends_completed_manual_weeks() {
    let store = Store::in_memory().expect("store");
    let account_id = anchor_account(&store, at(2024, 6, 6, 18, 0, 0));
    claude_event(
        &store,
        &account_id,
        at(2024, 6, 10, 12, 0, 0),
        "historical",
        simple_usage(4),
        4,
    );
    let contributions = store
        .quota_cycle_contributions(&QuotaQuery::default(), "device-weekly")
        .expect("contributions");
    assert!(contributions
        .iter()
        .any(|contribution| contribution.provider == "claude_code"
            && contribution.provider_account_id == account_id));
}

#[test]
fn manual_contributions_honor_source_limit_and_time_filters() {
    let store = Store::in_memory().expect("store");
    let account_id = anchor_account(&store, at(2026, 1, 8, 18, 0, 0));
    claude_event(
        &store,
        &account_id,
        at(2026, 1, 10, 12, 0, 0),
        "first-week",
        simple_usage(4),
        4,
    );
    claude_event(
        &store,
        &account_id,
        at(2026, 1, 20, 12, 0, 0),
        "second-week",
        simple_usage(6),
        6,
    );
    let source_id = store
        .events()
        .expect("events")
        .into_iter()
        .find(|event| event.provider == "claude_code")
        .expect("claude event")
        .source_id;
    let all = store
        .quota_cycle_contributions(&QuotaQuery::default(), "device-weekly")
        .expect("all");
    let claude = all
        .iter()
        .filter(|contribution| contribution.provider == "claude_code")
        .count();
    assert_eq!(claude, 2);

    let other_source = store
        .quota_cycle_contributions(
            &QuotaQuery {
                source_id: Some(SourceId("other-source".to_string())),
                ..QuotaQuery::default()
            },
            "device-weekly",
        )
        .expect("other source");
    assert!(other_source
        .iter()
        .all(|contribution| contribution.provider != "claude_code"));

    let matching_source = store
        .quota_cycle_contributions(
            &QuotaQuery {
                source_id: Some(source_id),
                ..QuotaQuery::default()
            },
            "device-weekly",
        )
        .expect("matching source");
    assert_eq!(
        matching_source
            .iter()
            .filter(|contribution| contribution.provider == "claude_code")
            .count(),
        2
    );

    let limited = store
        .quota_cycle_contributions(
            &QuotaQuery {
                limit_id: Some("five_hour".to_string()),
                ..QuotaQuery::default()
            },
            "device-weekly",
        )
        .expect("limit");
    assert!(limited
        .iter()
        .all(|contribution| contribution.provider != "claude_code"));

    let future = store
        .quota_cycle_contributions(
            &QuotaQuery {
                from: Some(at(2030, 1, 1, 0, 0, 0)),
                ..QuotaQuery::default()
            },
            "device-weekly",
        )
        .expect("future");
    assert!(future
        .iter()
        .all(|contribution| contribution.provider != "claude_code"));

    let before = store
        .quota_cycle_contributions(
            &QuotaQuery {
                to: Some(at(2020, 1, 1, 0, 0, 0)),
                ..QuotaQuery::default()
            },
            "device-weekly",
        )
        .expect("before");
    assert!(before
        .iter()
        .all(|contribution| contribution.provider != "claude_code"));

    let second_only = store
        .quota_cycle_contributions(
            &QuotaQuery {
                from: Some(at(2026, 1, 16, 0, 0, 0)),
                to: Some(at(2026, 1, 21, 0, 0, 0)),
                ..QuotaQuery::default()
            },
            "device-weekly",
        )
        .expect("second week");
    let resets = second_only
        .iter()
        .filter(|contribution| contribution.provider == "claude_code")
        .map(|contribution| contribution.representative_reset)
        .collect::<Vec<_>>();
    assert_eq!(resets, vec![at(2026, 1, 22, 18, 0, 0)]);
}

fn query_plan(store: &Store, sql: &str) -> String {
    let explain = format!("EXPLAIN QUERY PLAN {sql}");
    let mut statement = store.conn.prepare(&explain).expect("explain");
    let bindings = (0..statement.parameter_count())
        .map(|_| rusqlite::types::Null)
        .collect::<Vec<_>>();
    let rows = statement
        .query_map(rusqlite::params_from_iter(bindings), |row| {
            row.get::<_, String>(3)
        })
        .expect("plan")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("plan rows");
    rows.join(" | ")
}
