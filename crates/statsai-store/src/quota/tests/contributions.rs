//! Cycle contributions read only what can reach them: attributed weekly windows,
//! and the events of each cycle's boundary days. These tests hold them to the
//! v0.7.2 reader, which rebuilt every window from the full observation JSON and
//! read every event between the first cycle's start and the last one's end.

use super::support::*;
use super::*;
use chrono::TimeZone;
use std::time::Instant;

fn at(year: i32, month: u32, day: u32, hour: u32, min: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, min, 0)
        .single()
        .expect("timestamp")
}

/// `quota_cycle_contributions` as v0.7.2 computed it: every window rebuilt from
/// `quota_observations`, then every event of every provider in the span of all
/// cycles. The reference the faster reader must match.
fn reference_contributions(
    store: &Store,
    query: &QuotaQuery,
    device_id: &str,
) -> Result<Vec<QuotaCycleContributionV1>> {
    let mut pending = Vec::new();
    for item in store.reconstruct_quota_windows(query)? {
        if item.window.window_minutes != QUOTA_WEEKLY_WINDOW_MINUTES {
            continue;
        }
        let Some(provider_account_id) = item.window.provider_account_id.clone() else {
            continue;
        };
        let Some(anchor) = item.window.change_points.first() else {
            continue;
        };
        let contribution_id = format!(
            "quota_cycle_{}",
            &hash_text(&format!(
                "quota_cycle_contribution.v1:{device_id}:{}:{}:{}:{}:{}",
                item.window.provider,
                provider_account_id.0,
                item.window.limit_id.as_deref().unwrap_or("default"),
                item.window.window_minutes,
                anchor.point_fingerprint
            ))[..32]
        );
        pending.push(PendingQuotaCycleContribution {
            contribution_id,
            provider_account_id,
            reconstructed: item,
        });
    }
    let bounds = effective_cycle_bounds(
        pending
            .iter()
            .map(|item| &item.reconstructed.window)
            .collect(),
    );
    let bounds_of = |item: &PendingQuotaCycleContribution| {
        bounds
            .get(&item.reconstructed.window.window_id)
            .copied()
            .unwrap_or(CycleBounds {
                start: item.reconstructed.window.inferred_start,
                end: item.reconstructed.window.representative_reset,
            })
    };
    let mut slices = HashMap::<String, Vec<QuotaUsageSliceV1>>::new();
    if !pending.is_empty() {
        let first_start = pending.iter().map(|item| bounds_of(item).start).min();
        let last_end = pending
            .iter()
            .map(|item| bounds_of(item).end)
            .max()
            .expect("non-empty");
        let mut builders = pending
            .iter()
            .map(|item| {
                let bounds = bounds_of(item);
                (
                    item.reconstructed.window.window_id.clone(),
                    boundary_slice_builders(
                        bounds.start,
                        bounds.end,
                        &item.reconstructed.window.provider,
                        &item.provider_account_id,
                    ),
                )
            })
            .collect::<Vec<_>>();
        for event in store.events_in_period(first_start, last_end)? {
            let Some(account_id) = event.provider_account_id.as_ref() else {
                continue;
            };
            for (_, builders) in &mut builders {
                if event.provider != builders.provider || account_id != &builders.account_id {
                    continue;
                }
                for slice in &mut builders.slices {
                    if event.session.started_at >= slice.period_start
                        && event.session.started_at < slice.period_end
                    {
                        slice.add_usage(&event.usage, event.cost.estimated_micro_usd());
                    }
                }
            }
        }
        for (window_id, builders) in builders {
            slices.insert(window_id, builders.slices);
        }
    }
    let mut contributions = pending
        .into_iter()
        .map(|item| QuotaCycleContributionV1 {
            schema_version: QUOTA_CYCLE_CONTRIBUTION_SCHEMA_VERSION.to_string(),
            boundary_slices: slices
                .remove(&item.reconstructed.window.window_id)
                .unwrap_or_default(),
            contribution_id: item.contribution_id,
            provider: item.reconstructed.window.provider,
            provider_account_id: item.provider_account_id,
            limit_id: item.reconstructed.window.limit_id,
            window_minutes: item.reconstructed.window.window_minutes,
            representative_reset: item.reconstructed.window.representative_reset,
            representative_reset_epoch_seconds: item
                .reconstructed
                .window
                .representative_reset_epoch_seconds,
            has_schedule_overlap: item.reconstructed.window.has_schedule_overlap,
            daily_envelopes: item.reconstructed.daily_envelopes,
        })
        .collect::<Vec<_>>();
    contributions.extend(store.manual_weekly_contributions(query, device_id, Utc::now())?);
    Ok(contributions)
}

fn assert_matches_reference(store: &Store, query: &QuotaQuery) -> usize {
    let actual = store
        .quota_cycle_contributions(query, "device-a")
        .expect("contributions");
    let expected = reference_contributions(store, query, "device-a").expect("reference");
    assert_eq!(
        serde_json::to_value(&actual).expect("json"),
        serde_json::to_value(&expected).expect("json"),
        "contributions for {query:?} differ from the v0.7.2 reader"
    );
    actual.len()
}

fn codex_source(store: &Store, name: &str, account: Option<&str>) -> SourceId {
    let source = SourceLocation::local_adapter(
        "codex",
        "codex-local-jsonl",
        "test",
        &Path::new("/tmp/quota-parity").join(name),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    if let Some(account) = account {
        let now = Utc::now();
        store
            .upsert_source_account_assignment(&SourceAccountAssignment {
                schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
                assignment_id: SourceAccountAssignmentId(format!("assignment-{name}")),
                source_id: source.source_id.clone(),
                provider: "codex".to_string(),
                provider_account_id: ProviderAccountId(account.to_string()),
                started_at: at(2020, 1, 1, 0, 0),
                ended_at: None,
                record_source: IdentitySource::UserConfigured,
                verified_at: Some(now),
                created_at: now,
                updated_at: now,
            })
            .expect("assignment");
    }
    source.source_id
}

/// One observation reporting `windows`, each `(slot, minutes, used, reset)`.
fn observation(
    source_id: &SourceId,
    id: &str,
    fingerprint: &str,
    observed_at: DateTime<Utc>,
    windows: &[(&str, u64, f64, DateTime<Utc>)],
) -> QuotaObservationRecordV1 {
    let (slot, minutes, used, reset) = windows[0];
    let mut record = sample_record(
        source_id.clone(),
        id,
        fingerprint,
        observed_at,
        reset.timestamp(),
        slot,
        minutes,
        used,
    );
    record.windows = windows
        .iter()
        .map(|&(slot, minutes, used, reset)| QuotaWindowObservationV1 {
            schema_version: "quota_window_observation.v1".to_string(),
            window_observation_id: format!("window-{id}-{slot}"),
            observation_id: id.to_string(),
            provider_slot: slot.to_string(),
            limit_id: Some("subscription".to_string()),
            window_minutes: minutes,
            used_percent: used,
            resets_at: reset,
            resets_at_epoch_seconds: reset.timestamp(),
        })
        .collect();
    record
}

fn event(
    store: &Store,
    provider: &str,
    source_id: &SourceId,
    account: Option<&str>,
    started_at: DateTime<Utc>,
    record_id: &str,
    tokens: u64,
) {
    let account_id = ProviderAccountId(account.unwrap_or("none").to_string());
    let mut event = sample_usage_event(
        source_id,
        &account_id,
        started_at,
        record_id,
        tokens,
        tokens / 2,
        tokens / 4,
        tokens / 8,
        i64::try_from(tokens * 10).expect("cost"),
    );
    event.provider = provider.to_string();
    event.provider_account_id = account.map(|account| ProviderAccountId(account.to_string()));
    event.event_id = event_id(provider, source_id, record_id, None, started_at);
    store.insert_event(&event).expect("event");
}

/// Two accounts' weekly and five-hour schedules, an early reset, unattributed and
/// duplicated readings, a replay, and events on and off the boundary days, from
/// three providers.
fn mixed_store() -> (Store, SourceId) {
    let store = Store::in_memory().expect("store");
    let alpha = codex_source(&store, "alpha", Some("account-alpha"));
    let alpha_copy = codex_source(&store, "alpha-copy", Some("account-alpha"));
    let beta = codex_source(&store, "beta", Some("account-beta"));
    let loose = codex_source(&store, "loose", None);

    // Mid-day resets, so both ends of every cycle are partial UTC days.
    let alpha_resets = [
        at(2026, 3, 7, 14, 30),
        at(2026, 3, 14, 14, 30),
        at(2026, 3, 21, 14, 30),
    ];
    let mut records = Vec::new();
    for (week, reset) in alpha_resets.iter().enumerate() {
        let start = *reset - Duration::days(7);
        for step in 0..12i64 {
            let observed_at = start + Duration::hours(2 + step * 13);
            let used = 3.0 + 7.0 * step as f64;
            let id = format!("alpha-{week}-{step}");
            records.push(observation(
                &alpha,
                &id,
                &id,
                observed_at,
                &[
                    ("secondary", 10_080, used, *reset),
                    (
                        "primary",
                        300,
                        (step * 9 % 100) as f64,
                        observed_at + Duration::hours(3),
                    ),
                ],
            ));
        }
    }
    // Beta resets early in its second week: two overlapping schedules.
    let beta_first = at(2026, 3, 10, 9, 15);
    let beta_early = at(2026, 3, 15, 22, 45);
    for step in 0..8i64 {
        let observed_at = beta_first - Duration::days(6) + Duration::hours(step * 17);
        let id = format!("beta-a-{step}");
        records.push(observation(
            &beta,
            &id,
            &id,
            observed_at,
            &[("secondary", 10_080, 10.0 + step as f64 * 8.0, beta_first)],
        ));
    }
    for step in 0..8i64 {
        let observed_at = beta_early - Duration::days(4) + Duration::hours(step * 11);
        let id = format!("beta-b-{step}");
        records.push(observation(
            &beta,
            &id,
            &id,
            observed_at,
            &[("secondary", 10_080, 1.0 + step as f64 * 5.0, beta_early)],
        ));
    }
    // A reading recorded long after its window reset: a replay, never a point.
    records.push(observation(
        &beta,
        "beta-replay",
        "beta-replay",
        beta_early + Duration::days(3),
        &[("secondary", 10_080, 99.0, beta_early)],
    ));
    // Unattributed readings, some copying attributed ones.
    for step in 0..6i64 {
        let observed_at = alpha_resets[1] - Duration::days(5) + Duration::hours(step * 9);
        let id = format!("loose-{step}");
        records.push(observation(
            &loose,
            &id,
            &format!("alpha-1-{step}"),
            observed_at,
            &[("secondary", 10_080, 2.0 + step as f64, alpha_resets[1])],
        ));
    }

    // Copies of one reading under one account that rank differently: the copy
    // with more windows wins.
    let dup_at = alpha_resets[2] - Duration::days(2);
    records.push(observation(
        &alpha_copy,
        "alpha-copy-rich",
        "dup-rich",
        dup_at,
        &[
            ("secondary", 10_080, 77.0, alpha_resets[2]),
            ("primary", 300, 12.0, dup_at + Duration::hours(2)),
        ],
    ));
    records.push(observation(
        &alpha,
        "alpha-copy-lean",
        "dup-rich",
        dup_at,
        &[("secondary", 10_080, 78.0, alpha_resets[2])],
    ));
    // A copy whose winner reports no weekly window at all: the weekly reading of
    // the loser must not survive.
    let shadow_at = alpha_resets[2] - Duration::days(1);
    records.push(observation(
        &alpha_copy,
        "alpha-shadow-winner",
        "dup-shadow",
        shadow_at,
        &[
            ("primary", 300, 40.0, shadow_at + Duration::hours(1)),
            ("tertiary", 300, 41.0, shadow_at + Duration::hours(2)),
        ],
    ));
    records.push(observation(
        &alpha,
        "alpha-shadow-loser",
        "dup-shadow",
        shadow_at,
        &[("secondary", 10_080, 99.5, alpha_resets[2])],
    ));
    // One reading copied under both accounts: each keeps its own.
    let shared_at = beta_first - Duration::days(1);
    records.push(observation(
        &alpha,
        "shared-alpha",
        "dup-shared",
        shared_at,
        &[("secondary", 10_080, 50.0, beta_first)],
    ));
    records.push(observation(
        &beta,
        "shared-beta",
        "dup-shared",
        shared_at,
        &[("secondary", 10_080, 50.0, beta_first)],
    ));
    // Equal copies: the later one in observation order wins.
    let tie_at = alpha_resets[0] - Duration::days(3);
    for id in ["tie-a", "tie-b"] {
        records.push(observation(
            &alpha_copy,
            id,
            "dup-tie",
            tie_at,
            &[("secondary", 10_080, 31.0, alpha_resets[0])],
        ));
    }
    store
        .upsert_quota_observations(&records)
        .expect("observations");
    // A linked copy outranks an unlinked one with more windows.
    store
        .conn
        .execute(
            "UPDATE quota_observations SET usage_event_id = 'linked-event'
             WHERE observation_id = 'alpha-copy-lean'",
            [],
        )
        .expect("link");

    let claude = SourceId("claude-source".to_string());
    let mut events = 0;
    for day in 0..24 {
        for hour in [0, 9, 14, 23] {
            let started_at = at(2026, 2, 26, hour, 30) + Duration::days(day);
            events += 1;
            for (provider, source, account) in [
                ("codex", &alpha, Some("account-alpha")),
                ("codex", &beta, Some("account-beta")),
                ("codex", &loose, None),
                ("claude_code", &claude, Some("account-alpha")),
                ("cursor", &claude, Some("account-beta")),
            ] {
                event(
                    &store,
                    provider,
                    source,
                    account,
                    started_at,
                    &format!("{provider}-{}-{events}", source.0),
                    100 + events as u64,
                );
            }
        }
    }
    // On a reset instant and on a cycle's first instant: the slice ends are
    // exclusive and its starts inclusive.
    event(
        &store,
        "codex",
        &alpha,
        Some("account-alpha"),
        alpha_resets[0],
        "on-reset",
        5_000,
    );
    event(
        &store,
        "codex",
        &beta,
        Some("account-beta"),
        beta_first - Duration::days(7),
        "on-start",
        7_000,
    );
    store
        .upsert_weekly_reset_anchor(
            "claude_code",
            &ProviderAccountId("account-alpha".to_string()),
            at(2026, 3, 5, 18, 0),
        )
        .expect("anchor");
    (store, alpha)
}

#[test]
fn contributions_match_the_full_reader_for_every_query_shape() {
    let (store, alpha) = mixed_store();
    let all = assert_matches_reference(&store, &QuotaQuery::default());
    assert!(
        all >= 5,
        "the fixture yields too few cycles to compare: {all}"
    );
    for query in [
        QuotaQuery {
            provider: Some("codex".to_string()),
            ..QuotaQuery::default()
        },
        QuotaQuery {
            provider: Some("claude_code".to_string()),
            ..QuotaQuery::default()
        },
        QuotaQuery {
            provider: Some("cursor".to_string()),
            ..QuotaQuery::default()
        },
        QuotaQuery {
            provider_account_id: Some(ProviderAccountId("account-beta".to_string())),
            ..QuotaQuery::default()
        },
        QuotaQuery {
            limit_id: Some("subscription".to_string()),
            ..QuotaQuery::default()
        },
        QuotaQuery {
            limit_id: Some("other".to_string()),
            ..QuotaQuery::default()
        },
        QuotaQuery {
            source_id: Some(alpha.clone()),
            ..QuotaQuery::default()
        },
        QuotaQuery {
            from: Some(at(2026, 3, 12, 0, 0)),
            to: Some(at(2026, 3, 16, 0, 0)),
            ..QuotaQuery::default()
        },
    ] {
        assert_matches_reference(&store, &query);
    }
}

#[test]
fn provider_scoped_contributions_are_the_unscoped_ones_filtered() {
    let (store, _) = mixed_store();
    let all = store
        .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
        .expect("all");
    for provider in ["codex", "claude_code", "cursor"] {
        let scoped = store
            .quota_cycle_contributions(
                &QuotaQuery {
                    provider: Some(provider.to_string()),
                    ..QuotaQuery::default()
                },
                "device-a",
            )
            .expect("scoped");
        let filtered = all
            .iter()
            .filter(|contribution| contribution.provider == provider)
            .collect::<Vec<_>>();
        assert_eq!(
            serde_json::to_value(&scoped).expect("json"),
            serde_json::to_value(&filtered).expect("json"),
            "{provider}"
        );
    }
}

#[test]
fn collapsed_copies_rank_as_the_full_reader_ranks_them() {
    let (store, _) = mixed_store();
    let reader_query = QuotaQuery::default();
    let mut full = store
        .quota_observations(&reader_query, true)
        .expect("full reader");
    full.retain_mut(|record| {
        record
            .windows
            .retain(|window| window.window_minutes == QUOTA_WEEKLY_WINDOW_MINUTES);
        record.observation.provider_account_id.is_some() && !record.windows.is_empty()
    });
    let lean = store
        .attributed_weekly_quota_observations(&reader_query)
        .expect("lean reader");
    let summary = |records: &[QuotaObservationRecordV1]| {
        records
            .iter()
            .map(|record| {
                (
                    record.observation.observation_id.clone(),
                    record.observation.provider_account_id.clone(),
                    record.observation.observed_at,
                    record
                        .windows
                        .iter()
                        .map(|window| {
                            (
                                window.window_observation_id.clone(),
                                window.used_percent.to_bits(),
                                window.resets_at_epoch_seconds,
                                window.limit_id.clone(),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(summary(&lean), summary(&full));
    let ids = lean
        .iter()
        .map(|record| record.observation.observation_id.as_str())
        .collect::<HashSet<_>>();
    // The linked copy beats the one with more windows; the shadow copy with no
    // weekly window wins and takes the weekly reading with it; equal copies keep
    // the later one; each account keeps its copy of a shared reading.
    assert!(ids.contains("alpha-copy-lean") && !ids.contains("alpha-copy-rich"));
    assert!(!ids.contains("alpha-shadow-loser"));
    assert!(ids.contains("tie-b") && !ids.contains("tie-a"));
    assert!(ids.contains("shared-alpha") && ids.contains("shared-beta"));
    assert!(!ids.iter().any(|id| id.starts_with("loose-")));
}

#[test]
fn codex_contributions_read_only_the_boundary_days_of_their_own_account() {
    let (store, alpha) = mixed_store();
    let before = store
        .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
        .expect("contributions");
    let boundary_days = before
        .iter()
        .filter(|contribution| contribution.provider == "codex")
        .flat_map(|contribution| {
            contribution.boundary_slices.iter().map(move |slice| {
                (
                    contribution.provider_account_id.0.clone(),
                    slice.period_start.date_naive(),
                )
            })
        })
        .collect::<HashSet<_>>();
    assert!(!boundary_days.is_empty());

    // Plant an unparseable event on every hour of the span the old reader read
    // that no boundary slice can take: interior days, other accounts and other
    // providers on boundary days, and unattributed events. Any one of them,
    // read, fails the whole call.
    let mut planted = 0;
    let mut hour = at(2026, 2, 20, 0, 0);
    while hour < at(2026, 3, 28, 0, 0) {
        for (provider, account) in [
            ("codex", Some("account-alpha")),
            ("codex", Some("account-beta")),
            ("codex", None),
            ("cursor", Some("account-alpha")),
        ] {
            let on_own_boundary = provider == "codex"
                && account.is_some_and(|account| {
                    boundary_days.contains(&(account.to_string(), hour.date_naive()))
                });
            if on_own_boundary {
                continue;
            }
            planted += 1;
            store
                .conn
                .execute(
                    "INSERT INTO usage_events (
                       event_id, provider, source_id, provider_account_id, started_at,
                       total_tokens, semantic_fingerprint, payload
                     ) VALUES (?1, ?2, ?3, ?4, ?5, 0, ?1, 'not-json')",
                    params![
                        format!("planted-{planted}"),
                        provider,
                        &alpha.0,
                        account,
                        hour.to_rfc3339()
                    ],
                )
                .expect("planted event");
        }
        hour += Duration::hours(1);
    }
    assert!(planted > 1_000);

    let after = store
        .quota_cycle_contributions(
            &QuotaQuery {
                provider: Some("codex".to_string()),
                ..QuotaQuery::default()
            },
            "device-a",
        )
        .expect("only boundary-day events of the cycle's own account are read");
    let codex_before = before
        .iter()
        .filter(|contribution| contribution.provider == "codex")
        .collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_value(&after).expect("json"),
        serde_json::to_value(&codex_before).expect("json")
    );
    assert!(
        reference_contributions(
            &store,
            &QuotaQuery {
                provider: Some("codex".to_string()),
                ..QuotaQuery::default()
            },
            "device-a"
        )
        .is_err(),
        "the v0.7.2 reader parses the planted events, so they lie inside the span it read"
    );
}

#[test]
fn boundary_day_reads_stay_on_the_provider_account_index() {
    let store = Store::in_memory().expect("store");
    let sql = super::super::weekly::boundary_day_events_sql(3, false);
    let mut statement = store
        .conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("explain");
    let bindings = (0..statement.parameter_count())
        .map(|_| rusqlite::types::Null)
        .collect::<Vec<_>>();
    let plan = statement
        .query_map(rusqlite::params_from_iter(bindings), |row| {
            row.get::<_, String>(3)
        })
        .expect("plan")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("plan rows")
        .join(" | ");
    assert_eq!(
        plan.matches("usage_events_provider_account_started_idx (provider=? AND provider_account_id=? AND started_at>? AND started_at<?)")
            .count(),
        3,
        "{plan}"
    );
}

#[test]
fn the_weekly_reader_uses_covering_indexes_for_observations() {
    let store = Store::in_memory().expect("store");
    for (sql, index) in [
        (
            "SELECT rowid, observation_id, provider, provider_account_id, observed_at
             FROM quota_observations WHERE provider_account_id IS NOT NULL",
            "COVERING INDEX quota_observations_scope_idx",
        ),
        (
            "SELECT rowid, observation_id, provider, provider_account_id, observed_at
             FROM quota_observations WHERE provider_account_id IS NOT NULL AND provider = ?1",
            "COVERING INDEX quota_observations_scope_idx",
        ),
        (
            "SELECT semantic_fingerprint, observation_id FROM quota_observations
             ORDER BY semantic_fingerprint",
            "COVERING INDEX quota_observations_semantic_idx",
        ),
        (
            "SELECT rowid FROM quota_observations WHERE usage_event_id IS NOT NULL",
            "COVERING INDEX quota_observations_usage_event_idx",
        ),
    ] {
        let mut statement = store
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("explain");
        let bindings = (0..statement.parameter_count())
            .map(|_| rusqlite::types::Null)
            .collect::<Vec<_>>();
        let plan = statement
            .query_map(rusqlite::params_from_iter(bindings), |row| {
                row.get::<_, String>(3)
            })
            .expect("plan")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("plan rows")
            .join(" | ");
        assert!(plan.contains(index), "{sql}: {plan}");
    }
}

/// Fills `store` with `accounts` Codex accounts of weekly and five-hour readings,
/// half an hour apart, and with events of three providers, directly in SQL.
fn seed_heavy_profile(store: &Store, accounts: usize, observations_per_account: usize) {
    let start = at(2025, 1, 6, 3, 17);
    store.conn.execute_batch("BEGIN").expect("begin");
    store
        .conn
        .execute(
            "INSERT INTO quota_payloads (payload_hash, provider, payload, created_at)
             VALUES ('shared', 'codex', '{}', '2025-01-01T00:00:00+00:00')",
            [],
        )
        .expect("payload");
    let mut observation = store
        .conn
        .prepare(
            "INSERT INTO quota_observations (
               observation_id, semantic_fingerprint, provider, source_id,
               provider_account_id, observed_at, source_file_path_hash,
               source_record_id, usage_event_id, usage_link_kind, payload_hash, payload
             ) VALUES (?1, ?1, 'codex', ?2, ?3, ?4, 'file', ?1, NULL, 'none', 'shared', ?5)",
        )
        .expect("observation statement");
    let mut window = store
        .conn
        .prepare(
            "INSERT INTO quota_window_observations (
               window_observation_id, observation_id, provider_slot, limit_id,
               window_minutes, used_percent, resets_at, payload
             ) VALUES (?1, ?2, ?3, 'codex', ?4, ?5, ?6, ?7)",
        )
        .expect("window statement");
    for account in 0..accounts {
        let account_id = format!("account-{account}");
        let source_id = SourceId(format!("source-{account}"));
        // Weekly resets drift a few minutes per account so the cycles differ.
        let phase = Duration::minutes(37 * account as i64);
        for step in 0..observations_per_account {
            let observed_at = start + phase + Duration::minutes(30 * step as i64);
            let week = (observed_at - start - phase).num_days() / 7;
            let weekly_reset = start + phase + Duration::days(7 * (week + 1));
            let five_hour_reset =
                observed_at + Duration::minutes(300 - (observed_at.timestamp() / 60) % 300);
            let id = format!("obs-{account}-{step}");
            let record = QuotaObservationV1 {
                observation_id: id.clone(),
                semantic_fingerprint: id.clone(),
                provider_account_id: Some(ProviderAccountId(account_id.clone())),
                observed_at,
                ..sample_record(
                    source_id.clone(),
                    &id,
                    &id,
                    observed_at,
                    0,
                    "secondary",
                    10_080,
                    0.0,
                )
                .observation
            };
            observation
                .execute(params![
                    &id,
                    &source_id.0,
                    &account_id,
                    observed_at.to_rfc3339(),
                    serde_json::to_string(&record).expect("observation json"),
                ])
                .expect("observation");
            let elapsed = (observed_at - (weekly_reset - Duration::days(7))).num_minutes() as f64;
            for (slot, minutes, used, reset) in [
                ("secondary", 10_080i64, elapsed / 100.8, weekly_reset),
                ("primary", 300, ((step * 7) % 100) as f64, five_hour_reset),
            ] {
                let window_id = format!("{id}-{slot}");
                let payload = serde_json::to_string(&QuotaWindowObservationV1 {
                    schema_version: "quota_window_observation.v1".to_string(),
                    window_observation_id: window_id.clone(),
                    observation_id: id.clone(),
                    provider_slot: slot.to_string(),
                    limit_id: Some("codex".to_string()),
                    window_minutes: minutes as u64,
                    used_percent: used,
                    resets_at: reset,
                    resets_at_epoch_seconds: reset.timestamp(),
                })
                .expect("window json");
                window
                    .execute(params![
                        &window_id,
                        &id,
                        slot,
                        minutes,
                        used,
                        reset.timestamp(),
                        payload
                    ])
                    .expect("window");
            }
        }
    }
    drop(observation);
    drop(window);
    let span_hours = (observations_per_account / 2) as i64;
    let mut written = 0usize;
    for hour in 0..span_hours {
        let started_at = start + Duration::hours(hour) + Duration::minutes(11);
        for (provider, account) in [
            ("codex", hour as usize % accounts),
            ("claude_code", 0),
            ("cursor", 1 % accounts),
        ] {
            written += 1;
            let account_id = ProviderAccountId(format!("account-{account}"));
            let mut event = sample_usage_event(
                &SourceId(format!("source-{account}")),
                &account_id,
                started_at,
                &format!("event-{written}"),
                1_000,
                500,
                100,
                10,
                1_234,
            );
            event.provider = provider.to_string();
            store.insert_event(&event).expect("event");
        }
    }
    store.conn.execute_batch("COMMIT").expect("commit");
}

fn time_heavy_profile(accounts: usize, observations_per_account: usize) {
    let store = Store::in_memory().expect("store");
    seed_heavy_profile(&store, accounts, observations_per_account);
    let started = Instant::now();
    let reference =
        reference_contributions(&store, &QuotaQuery::default(), "device-a").expect("reference");
    let reference_elapsed = started.elapsed();
    let started = Instant::now();
    let contributions = store
        .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
        .expect("contributions");
    let elapsed = started.elapsed();
    eprintln!(
        "quota_cycle_contributions(&QuotaQuery::default()): {} observations, {} contributions: \
         {elapsed:?} (v0.7.2 reader: {reference_elapsed:?})",
        accounts * observations_per_account,
        contributions.len()
    );
    assert!(contributions.len() >= accounts);
    assert_eq!(
        serde_json::to_value(&contributions).expect("json"),
        serde_json::to_value(&reference).expect("json")
    );
}

#[test]
fn a_heavy_profile_matches_the_full_reader() {
    // Small enough for a debug build; `heavy_profile_timing` is the full size.
    time_heavy_profile(3, 1_500);
}

/// A heavy store: 150k observations across five Codex accounts, each with a
/// weekly and a five-hour window, and 45k events of three providers. Run it in
/// release:
///
/// `cargo test -p statsai-store --release --lib -- --ignored heavy_profile_timing --nocapture`
#[test]
#[ignore = "the full-size profile; run in release with --ignored --nocapture"]
fn heavy_profile_timing() {
    time_heavy_profile(5, 30_000);
}
