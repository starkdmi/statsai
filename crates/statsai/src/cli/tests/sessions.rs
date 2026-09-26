use super::support::*;
use super::*;

#[test]
fn sessions_cli_parses_filters() {
    let cli = Cli::try_parse_from([
        "statsai",
        "sessions",
        "--from",
        "2026-06-01",
        "--to",
        "2026-06-15",
        "--provider",
        "codex",
        "--project",
        "statsai",
        "--sort",
        "tokens",
        "--limit",
        "10",
        "--json",
    ])
    .expect("parse");
    match cli.command {
        Command::Sessions(command) => {
            assert_eq!(command.from.as_deref(), Some("2026-06-01"));
            assert_eq!(command.to.as_deref(), Some("2026-06-15"));
            assert_eq!(command.provider.as_deref(), Some("codex"));
            assert_eq!(command.project.as_deref(), Some("statsai"));
            assert_eq!(command.sort, SessionSortArg::Tokens);
            assert_eq!(command.limit, 10);
            assert!(command.json);
        }
        other => panic!("unexpected command {other:?}"),
    }
}

#[test]
fn sessions_view_reads_rollups_for_the_default_week() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/sessions-cli"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc
        .with_ymd_and_hms(2026, 6, 20, 12, 0, 0)
        .single()
        .expect("now");
    let mut recent = test_event(
        "codex",
        &source,
        now - Duration::days(1),
        None,
        TokenParts::total(40),
    );
    recent.session.session_id = "session_recent".to_string();
    recent.session.title = Some("Recent indexing work".to_string());
    recent.model = Some(ModelInfo {
        normalized_name: Some("gpt-5".to_string()),
        ..ModelInfo::default()
    });
    let mut old = test_event(
        "codex",
        &source,
        now - Duration::days(30),
        None,
        TokenParts::total(9),
    );
    old.session.session_id = "session_old".to_string();
    store.insert_event(&recent).expect("recent");
    store.insert_event(&old).expect("old");

    let command = SessionsCommand {
        from: None,
        to: None,
        provider: Some("codex".to_string()),
        project: None,
        sort: SessionSortArg::Started,
        limit: 50,
        json: true,
    };
    let view = sessions_view(&command, &store, now).expect("view");
    assert_eq!(view.label, "last 7 days");
    assert_eq!(view.stats.sessions, 1);
    assert_eq!(view.stats.total_tokens, 40);
    assert_eq!(view.stats.top_model.as_deref(), Some("gpt-5"));
    assert_eq!(view.sessions.len(), 1);
    assert_eq!(
        view.sessions[0].title.as_deref(),
        Some("Recent indexing work")
    );
    assert_eq!(
        view.sessions[0].title_source,
        Some(statsai_core::SessionTitleSource::Event)
    );
}

#[test]
fn session_durations_switch_to_days_past_two_days() {
    use crate::cli::sessions::format_duration;
    assert_eq!(format_duration(None), "unknown");
    assert_eq!(format_duration(Some(45)), "45s");
    assert_eq!(format_duration(Some(3 * 60 + 5)), "3m 05s");
    assert_eq!(format_duration(Some(47 * 3600 + 59 * 60)), "47h 59m");
    assert_eq!(format_duration(Some(498 * 3600 + 15 * 60)), "20d 18h");
}
