use super::*;
use crate::{AccountEvidenceScan, CLAUDE_CODE_PROVIDER};
use statsai_core::{
    hash_text, provider_account_id_from_identity, Confidence, CONVERSATION_ACTIVITY_GRACE_SECONDS,
};

const OLD_ACCOUNT: &str = "old-account-uuid";
const NEW_ACCOUNT: &str = "new-account-uuid";
const HOUR_MILLIS: i64 = 3_600_000;
const BASE_MILLIS: i64 = 1_786_104_000_000;

fn at(hours: i64) -> i64 {
    BASE_MILLIS + hours * HOUR_MILLIS
}

/// When the next account takes over from a stretch that ended at `hours`.
fn after(hours: i64) -> i64 {
    at(hours) + CONVERSATION_ACTIVITY_GRACE_SECONDS * 1000
}

fn utc(millis: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(millis)
        .single()
        .expect("timestamp")
}

fn write_record(
    root: &Path,
    account: &str,
    organization: &str,
    record: &str,
    cli_session_id: &str,
    created_at: i64,
    last_activity_at: i64,
) {
    let directory = root.join(account).join(organization);
    std::fs::create_dir_all(&directory).expect("organization directory");
    std::fs::write(
        directory.join(format!("local_{record}.json")),
        serde_json::json!({
            "sessionId": format!("local_{record}"),
            "cliSessionId": cli_session_id,
            "createdAt": created_at,
            "lastActivityAt": last_activity_at,
            "title": "ignored",
        })
        .to_string(),
    )
    .expect("session record");
}

fn record(account: &str, cli_session_id: &str, created: i64, last: i64) -> DesktopSessionRecord {
    DesktopSessionRecord {
        account_uuid: account.to_string(),
        desktop_session_id: format!("local-{cli_session_id}"),
        cli_session_id: cli_session_id.to_string(),
        prior_cli_session_ids: Vec::new(),
        created_at: utc(at(created)),
        last_activity_at: utc(at(last)),
    }
}

/// The old account signed in long before the fixtures; the new one at hour 9.
fn sessions(records: Vec<DesktopSessionRecord>) -> DesktopSessions {
    DesktopSessions {
        records,
        account_first_seen: [(OLD_ACCOUNT, -100), (NEW_ACCOUNT, 9)]
            .into_iter()
            .map(|(account, hours)| (account.to_string(), utc(at(hours))))
            .collect(),
        ..DesktopSessions::default()
    }
}

type Takeover = (String, String, Option<DateTime<Utc>>, Confidence);

fn takeovers(sessions: &DesktopSessions) -> Vec<Takeover> {
    claude_desktop_session_takeovers(sessions)
        .into_iter()
        .map(|takeover| {
            (
                takeover.cli_session_id,
                takeover.account_uuid,
                takeover.active_from,
                takeover.confidence,
            )
        })
        .collect()
}

fn takeover(
    cli_session_id: &str,
    account: &str,
    active_from: Option<i64>,
    confidence: Confidence,
) -> Takeover {
    (
        cli_session_id.to_string(),
        account.to_string(),
        active_from.map(utc),
        confidence,
    )
}

#[test]
fn claude_desktop_sessions_bind_to_the_account_directory_that_holds_them() {
    let sessions = sessions(vec![
        record(OLD_ACCOUNT, "cli-old", 0, 1),
        record(NEW_ACCOUNT, "cli-new", 10, 11),
    ]);

    assert_eq!(
        takeovers(&sessions),
        vec![
            takeover("cli-new", NEW_ACCOUNT, None, Confidence::High),
            takeover("cli-old", OLD_ACCOUNT, None, Confidence::High),
        ]
    );
}

#[test]
fn claude_desktop_untouched_copies_stay_with_the_account_that_ran_them() {
    let sessions = sessions(vec![
        record(OLD_ACCOUNT, "cli-1", 0, 1),
        record(NEW_ACCOUNT, "cli-1", 0, 1),
        record(NEW_ACCOUNT, "cli-2", 10, 11),
    ]);

    assert_eq!(
        takeovers(&sessions),
        vec![
            takeover("cli-1", OLD_ACCOUNT, None, Confidence::Medium),
            takeover("cli-2", NEW_ACCOUNT, None, Confidence::High),
        ]
    );
}

#[test]
fn claude_desktop_copy_continued_under_a_new_account_takes_over_after_the_original_ends() {
    let sessions = sessions(vec![
        record(OLD_ACCOUNT, "cli-1", 0, 2),
        record(NEW_ACCOUNT, "cli-1", 0, 20),
    ]);

    assert_eq!(
        takeovers(&sessions),
        vec![
            takeover("cli-1", OLD_ACCOUNT, None, Confidence::Medium),
            takeover("cli-1", NEW_ACCOUNT, Some(at(9)), Confidence::High),
        ]
    );
}

#[test]
fn claude_desktop_original_continued_after_switching_back_keeps_its_history() {
    // Copied to the new account, never used there, then continued by the old
    // account after switching back: the untouched copy must not claim it.
    let sessions = sessions(vec![
        record(OLD_ACCOUNT, "cli-1", 0, 30),
        record(NEW_ACCOUNT, "cli-1", 0, 2),
    ]);

    assert_eq!(
        takeovers(&sessions),
        vec![
            takeover("cli-1", OLD_ACCOUNT, None, Confidence::Medium),
            takeover("cli-1", OLD_ACCOUNT, Some(after(2)), Confidence::High),
        ]
    );
}

#[test]
fn claude_desktop_session_used_by_old_new_then_old_again_splits_at_first_sign_in() {
    // Old ran it to hour 2, the new account (first signed in at hour 9) continued
    // its copy to hour 12, and the old account continued again to hour 30. The
    // old record no longer shows where its first stretch ended, so the new
    // account's first sign-in bounds it.
    let sessions = sessions(vec![
        record(OLD_ACCOUNT, "cli-1", 0, 30),
        record(NEW_ACCOUNT, "cli-1", 0, 12),
        record(NEW_ACCOUNT, "cli-2", 9, 10),
    ]);

    assert_eq!(
        takeovers(&sessions),
        vec![
            takeover("cli-1", OLD_ACCOUNT, None, Confidence::Medium),
            takeover("cli-1", NEW_ACCOUNT, Some(at(9)), Confidence::Medium),
            takeover("cli-1", OLD_ACCOUNT, Some(after(12)), Confidence::High),
            takeover("cli-2", NEW_ACCOUNT, None, Confidence::High),
        ]
    );
}

#[test]
fn claude_desktop_copies_without_sign_in_evidence_are_left_unbound() {
    let sessions = DesktopSessions {
        records: vec![
            record(OLD_ACCOUNT, "cli-1", 0, 1),
            record(NEW_ACCOUNT, "cli-1", 0, 1),
        ],
        ..DesktopSessions::default()
    };

    assert!(takeovers(&sessions).is_empty());
}

#[test]
fn claude_desktop_one_account_in_several_organizations_binds_once() {
    let sessions = sessions(vec![
        record(OLD_ACCOUNT, "cli-1", 0, 1),
        record(OLD_ACCOUNT, "cli-1", 0, 4),
    ]);

    assert_eq!(
        takeovers(&sessions),
        vec![takeover("cli-1", OLD_ACCOUNT, None, Confidence::High)]
    );
}

#[test]
fn claude_desktop_skips_unreadable_and_unrelated_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let organization = dir.path().join(OLD_ACCOUNT).join("org-a");
    std::fs::create_dir_all(&organization).expect("organization directory");
    std::fs::write(organization.join("local_broken.json"), "{").expect("broken");
    std::fs::write(
        organization.join("local_no_cli.json"),
        r#"{"createdAt": 1786104000000}"#,
    )
    .expect("no cli session");
    std::fs::write(organization.join("archived-sessions.idx"), "ignored").expect("index");
    std::fs::write(dir.path().join("stray.json"), "{}").expect("stray file");

    let sessions = read_claude_desktop_sessions(dir.path());
    assert!(sessions.records.is_empty());
    // A record that cannot be read leaves its session incomplete; one that reads
    // cleanly but names no transcript does not.
    assert_eq!(
        sessions.incomplete_sessions,
        ["local_broken".to_string()].into_iter().collect()
    );
    assert!(!sessions.listing_failed);
    assert_eq!(
        read_claude_desktop_sessions(&dir.path().join("missing")),
        DesktopSessions::default()
    );
}

#[test]
fn claude_desktop_evidence_binds_transcript_sessions_and_reports_accounts() {
    let sessions = sessions(vec![
        record(OLD_ACCOUNT, "cli-1", 0, 2),
        record(NEW_ACCOUNT, "cli-1", 0, 20),
        record(NEW_ACCOUNT, "cli-2", 10, 11),
    ]);
    let dir = tempfile::tempdir().expect("tempdir");
    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "claude-code-local-jsonl",
        "0",
        &dir.path().join(".claude"),
        LocationOrigin::Default,
    );

    let mut scan = AccountEvidenceScan::default();
    bind_claude_desktop_sessions(&source, &sessions, &mut scan);

    let account = |uuid: &str| {
        provider_account_id_from_identity(CLAUDE_CODE_PROVIDER, Some(uuid), None)
            .expect("account id")
    };
    let mut bindings = scan
        .conversation_bindings
        .iter()
        .map(|binding| {
            assert_eq!(binding.binding_id, binding.derived_binding_id());
            assert_eq!(binding.source_id, source.source_id);
            assert_eq!(binding.turn_id_hash, None);
            (
                binding.conversation_id_hash.clone(),
                binding.provider_account_id.clone(),
                binding.active_from,
                binding.confidence.clone(),
            )
        })
        .collect::<Vec<_>>();
    bindings.sort_by_key(|binding| format!("{binding:?}"));
    let mut expected = vec![
        (
            hash_text("cli-1"),
            account(OLD_ACCOUNT),
            None,
            Confidence::Medium,
        ),
        (
            hash_text("cli-1"),
            account(NEW_ACCOUNT),
            Some(utc(at(9))),
            Confidence::High,
        ),
        (
            hash_text("cli-2"),
            account(NEW_ACCOUNT),
            None,
            Confidence::High,
        ),
    ];
    expected.sort_by_key(|binding| format!("{binding:?}"));
    assert_eq!(bindings, expected);

    let accounts = scan
        .accounts
        .iter()
        .map(|observed| {
            assert_eq!(observed.email, None);
            observed.provider_user_id.clone().expect("account uuid")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        accounts,
        vec![NEW_ACCOUNT.to_string(), OLD_ACCOUNT.to_string()]
    );
}

#[test]
fn claude_desktop_reader_collects_records_from_every_account_and_organization() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_record(dir.path(), OLD_ACCOUNT, "org-a", "1", "cli-1", at(0), at(2));
    write_record(dir.path(), OLD_ACCOUNT, "org-b", "2", "cli-2", at(3), at(4));
    write_record(
        dir.path(),
        NEW_ACCOUNT,
        "org-c",
        "1",
        "cli-1",
        at(0),
        at(20),
    );

    let sessions = read_claude_desktop_sessions(dir.path());
    let mut records = sessions.records.clone();
    records.sort_by(|left, right| {
        (&left.account_uuid, &left.cli_session_id)
            .cmp(&(&right.account_uuid, &right.cli_session_id))
    });

    let read = |account, desktop: &str, cli, created, last| DesktopSessionRecord {
        desktop_session_id: desktop.to_string(),
        ..record(account, cli, created, last)
    };
    assert_eq!(
        records,
        vec![
            read(NEW_ACCOUNT, "local_1", "cli-1", 0, 20),
            read(OLD_ACCOUNT, "local_1", "cli-1", 0, 2),
            read(OLD_ACCOUNT, "local_2", "cli-2", 3, 4),
        ]
    );
    // Directory creation times are missing on some filesystems.
    if std::fs::metadata(dir.path())
        .and_then(|metadata| metadata.created())
        .is_ok()
    {
        assert_eq!(
            sessions.account_first_seen.keys().collect::<Vec<_>>(),
            vec![NEW_ACCOUNT, OLD_ACCOUNT]
        );
    }
}

fn sessions_first_seen(
    records: Vec<DesktopSessionRecord>,
    first_seen: &[(&str, i64)],
) -> DesktopSessions {
    DesktopSessions {
        records,
        account_first_seen: first_seen
            .iter()
            .map(|(account, hours)| (account.to_string(), utc(at(*hours))))
            .collect(),
        ..DesktopSessions::default()
    }
}

#[test]
fn claude_desktop_takeover_starts_at_the_current_sign_in_not_the_first_one() {
    // Both accounts predate the session. The old account was seen again at hour
    // -20 (a session only it holds), after the new account's first sign-in at
    // -50, so the old account started the session at hour 0 and the new account
    // can only have taken over from its next evidence, the session at hour 9.
    let sessions = sessions_first_seen(
        vec![
            record(OLD_ACCOUNT, "cli-1", 0, 30),
            record(NEW_ACCOUNT, "cli-1", 0, 12),
            record(OLD_ACCOUNT, "cli-old", -20, -19),
            record(NEW_ACCOUNT, "cli-new", 9, 10),
        ],
        &[(OLD_ACCOUNT, -100), (NEW_ACCOUNT, -50)],
    );

    assert_eq!(
        takeovers(&sessions),
        vec![
            takeover("cli-1", OLD_ACCOUNT, None, Confidence::Medium),
            takeover("cli-1", NEW_ACCOUNT, Some(at(9)), Confidence::Medium),
            takeover("cli-1", OLD_ACCOUNT, Some(after(12)), Confidence::High),
            takeover("cli-new", NEW_ACCOUNT, None, Confidence::High),
            takeover("cli-old", OLD_ACCOUNT, None, Confidence::High),
        ]
    );
}

#[test]
fn claude_desktop_session_started_after_a_later_first_sign_in_belongs_to_that_account() {
    // Both accounts predate the session and nothing shows the old account signed
    // in again after the new one's first sign-in at -50. The new account was the
    // one signed in when the session began, so it ran the copied stretch; the
    // old account only holds what came after its record outgrew the copy.
    let sessions = sessions_first_seen(
        vec![
            record(OLD_ACCOUNT, "cli-1", 0, 30),
            record(NEW_ACCOUNT, "cli-1", 0, 12),
            record(NEW_ACCOUNT, "cli-new", 9, 10),
        ],
        &[(OLD_ACCOUNT, -100), (NEW_ACCOUNT, -50)],
    );

    assert_eq!(
        takeovers(&sessions),
        vec![
            takeover("cli-1", NEW_ACCOUNT, None, Confidence::Medium),
            takeover("cli-1", OLD_ACCOUNT, Some(after(12)), Confidence::High),
            takeover("cli-new", NEW_ACCOUNT, None, Confidence::High),
        ]
    );
}

#[test]
fn claude_desktop_accounts_taking_turns_each_keep_their_stretches() {
    // Sole-account sessions show B at hour 9, A at 18 and B again at 27; the
    // shared session's copied history must alternate the same way.
    let sessions = sessions_first_seen(
        vec![
            record(OLD_ACCOUNT, "cli-1", 0, 30),
            record(NEW_ACCOUNT, "cli-1", 0, 40),
            record(NEW_ACCOUNT, "cli-b1", 9, 10),
            record(OLD_ACCOUNT, "cli-a1", 18, 19),
            record(NEW_ACCOUNT, "cli-b2", 27, 28),
        ],
        &[(OLD_ACCOUNT, -100), (NEW_ACCOUNT, 5)],
    );

    let shared = takeovers(&sessions)
        .into_iter()
        .filter(|(cli, ..)| cli == "cli-1")
        .collect::<Vec<_>>();
    assert_eq!(
        shared,
        vec![
            takeover("cli-1", OLD_ACCOUNT, None, Confidence::Medium),
            takeover("cli-1", NEW_ACCOUNT, Some(at(5)), Confidence::Medium),
            takeover("cli-1", OLD_ACCOUNT, Some(at(18)), Confidence::Medium),
            takeover("cli-1", NEW_ACCOUNT, Some(at(27)), Confidence::Medium),
            takeover("cli-1", NEW_ACCOUNT, Some(after(30)), Confidence::High),
        ]
    );
}

#[test]
fn claude_desktop_every_stretch_records_where_its_activity_ends() {
    let sessions = sessions(vec![
        record(OLD_ACCOUNT, "cli-1", 0, 2),
        record(NEW_ACCOUNT, "cli-1", 0, 20),
    ]);

    let ranges = claude_desktop_session_takeovers(&sessions)
        .into_iter()
        .map(|takeover| {
            (
                takeover.confidence,
                takeover.observed_until,
                takeover.certain_until,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ranges,
        vec![
            (Confidence::Medium, Some(utc(at(2))), None),
            (Confidence::High, Some(utc(at(20))), Some(utc(at(20)))),
        ]
    );
}

#[test]
fn claude_desktop_reader_keeps_prior_transcripts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let organization = dir.path().join(NEW_ACCOUNT).join("org-a");
    std::fs::create_dir_all(&organization).expect("organization directory");
    std::fs::write(
        organization.join("local_1.json"),
        serde_json::json!({
            "sessionId": "local_1",
            "cliSessionId": "cli-current",
            "priorCliSessionIds": ["cli-first", 7, "", "cli-current", "cli-second"],
            "createdAt": at(0),
            "lastActivityAt": at(5),
        })
        .to_string(),
    )
    .expect("session record");

    let sessions = read_claude_desktop_sessions(dir.path());

    assert_eq!(
        sessions.records,
        vec![DesktopSessionRecord {
            desktop_session_id: "local_1".to_string(),
            prior_cli_session_ids: vec!["cli-first".to_string(), "cli-second".to_string()],
            ..record(NEW_ACCOUNT, "cli-current", 0, 5)
        }]
    );
}

#[test]
fn claude_desktop_resumed_session_binds_every_transcript_it_ran_in() {
    // The new account continued a copied session, and the app resumed it in a
    // fresh transcript; the old account's copy still names the first one.
    let mut continued = record(NEW_ACCOUNT, "cli-resumed", 0, 20);
    continued.desktop_session_id = "local-shared".to_string();
    continued.prior_cli_session_ids = vec!["cli-first".to_string()];
    let mut copy = record(OLD_ACCOUNT, "cli-first", 0, 2);
    copy.desktop_session_id = "local-shared".to_string();
    // A prior transcript another session currently runs in stays with it.
    let mut fork = record(NEW_ACCOUNT, "cli-fork", 10, 11);
    fork.prior_cli_session_ids = vec!["cli-other".to_string()];
    let other = record(OLD_ACCOUNT, "cli-other", 3, 4);
    let sessions = sessions(vec![continued, copy, fork, other]);

    let mut takeovers = takeovers(&sessions);
    takeovers.sort_by_key(|takeover| format!("{takeover:?}"));
    let mut expected = vec![
        takeover("cli-first", OLD_ACCOUNT, None, Confidence::Medium),
        takeover("cli-resumed", OLD_ACCOUNT, None, Confidence::Medium),
        takeover("cli-first", NEW_ACCOUNT, Some(at(9)), Confidence::High),
        takeover("cli-resumed", NEW_ACCOUNT, Some(at(9)), Confidence::High),
        takeover("cli-fork", NEW_ACCOUNT, None, Confidence::High),
        takeover("cli-other", OLD_ACCOUNT, None, Confidence::High),
    ];
    expected.sort_by_key(|takeover| format!("{takeover:?}"));
    assert_eq!(takeovers, expected);
}

#[test]
fn claude_desktop_copied_history_before_any_known_sign_in_stays_unbound() {
    // Only the new account has sign-in evidence, from hour 9. The copied
    // stretch from hour 0 is the new account's only from then on.
    let sessions = sessions_first_seen(
        vec![
            record(OLD_ACCOUNT, "cli-1", 0, 12),
            record(NEW_ACCOUNT, "cli-1", 0, 12),
        ],
        &[(NEW_ACCOUNT, 9)],
    );

    assert_eq!(
        takeovers(&sessions),
        vec![takeover(
            "cli-1",
            NEW_ACCOUNT,
            Some(at(9)),
            Confidence::Medium
        )]
    );
}

#[test]
fn claude_desktop_sessions_belong_only_to_the_default_configuration_directory() {
    let home = Path::new("/home/someone");
    let sessions_root =
        claude_desktop_sessions_root_from_home(home).expect("desktop sessions root");

    assert_eq!(
        claude_desktop_sessions_root_in(&home.join(".claude"), home),
        Some(sessions_root)
    );
    assert_eq!(
        claude_desktop_sessions_root_in(&home.join(".config/claude"), home),
        None
    );
    assert_eq!(
        claude_desktop_sessions_root_in(Path::new("/elsewhere/.claude"), home),
        None
    );
}

#[test]
fn claude_auth_dependencies_omit_desktop_sessions_for_other_sources() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    std::fs::create_dir_all(root.join("projects")).expect("projects");

    let dependencies = claude_auth_dependency_paths(&root, &LocationOrigin::Configured);

    assert!(!dependencies
        .iter()
        .any(|path| path.ends_with("claude-code-sessions")));
    assert!(!claude_verification_dependency_topology_changed(
        &root,
        &[dir.path().join("claude-code-sessions")]
    ));
}

#[test]
fn claude_desktop_session_with_an_unreadable_copy_yields_no_evidence() {
    // The old account's copy could not be read this scan: the new account's copy
    // must not look like the only holder, or it would bind for certain.
    let mut sessions = sessions(vec![
        record(NEW_ACCOUNT, "cli-1", 0, 20),
        record(OLD_ACCOUNT, "cli-2", 3, 4),
    ]);
    sessions
        .incomplete_sessions
        .insert("local-cli-1".to_string());

    assert_eq!(
        takeovers(&sessions),
        vec![takeover("cli-2", OLD_ACCOUNT, None, Confidence::High)]
    );
}

#[test]
fn claude_desktop_failed_listing_yields_no_evidence() {
    let mut sessions = sessions(vec![record(NEW_ACCOUNT, "cli-1", 0, 20)]);
    sessions.listing_failed = true;

    assert!(takeovers(&sessions).is_empty());
}

#[cfg(unix)]
#[test]
fn claude_desktop_reader_reports_a_directory_it_cannot_list() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    write_record(dir.path(), OLD_ACCOUNT, "org-a", "1", "cli-1", at(0), at(2));
    let locked = dir.path().join(NEW_ACCOUNT).join("org-b");
    std::fs::create_dir_all(&locked).expect("locked organization");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
        .expect("lock organization");
    let unlistable = std::fs::read_dir(&locked).is_err();

    let sessions = read_claude_desktop_sessions(dir.path());

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
        .expect("unlock organization");
    // Running as root can list anything; there is nothing to check then.
    if unlistable {
        assert!(sessions.listing_failed);
        assert!(claude_desktop_session_takeovers(&sessions).is_empty());
    }
}

#[test]
fn claude_desktop_quick_switch_takes_over_no_later_than_the_new_accounts_activity() {
    let minutes = |count: i64| count * 60 * 1000;
    // The new account continued the copy two minutes after the old account's
    // last activity: inside the grace, so its own activity bounds the takeover.
    let quick = sessions(vec![
        record(OLD_ACCOUNT, "cli-1", 0, 2),
        DesktopSessionRecord {
            last_activity_at: utc(at(2) + minutes(2)),
            ..record(NEW_ACCOUNT, "cli-1", 0, 2)
        },
    ]);
    let takeovers_with_ranges = |sessions: &DesktopSessions| {
        claude_desktop_session_takeovers(sessions)
            .into_iter()
            .map(|takeover| {
                if let (Some(from), Some(until)) = (takeover.active_from, takeover.certain_until) {
                    assert!(from <= until, "a range never starts after it ends");
                }
                (takeover.account_uuid, takeover.active_from)
            })
            .collect::<Vec<_>>()
    };

    assert_eq!(
        takeovers_with_ranges(&quick),
        vec![
            (OLD_ACCOUNT.to_string(), None),
            (NEW_ACCOUNT.to_string(), Some(utc(at(2) + minutes(2)))),
        ]
    );

    // Seen signed in a minute after the old account stopped, through a session
    // only it holds: that is when it took over.
    let mut signed_in_sooner = quick.clone();
    signed_in_sooner.records.push(DesktopSessionRecord {
        created_at: utc(at(2) + minutes(1)),
        last_activity_at: utc(at(2) + minutes(1)),
        ..record(NEW_ACCOUNT, "cli-own", 0, 0)
    });
    let shared = takeovers_with_ranges(&signed_in_sooner)
        .into_iter()
        .take(2)
        .collect::<Vec<_>>();
    assert_eq!(
        shared,
        vec![
            (OLD_ACCOUNT.to_string(), None),
            (NEW_ACCOUNT.to_string(), Some(utc(at(2) + minutes(1)))),
        ]
    );
}

#[test]
fn claude_desktop_takeover_never_starts_before_the_accounts_first_sign_in() {
    // The old account stopped at hour 2 and the new account first signed in at
    // hour 9. Usage in between, such as the session resumed in the CLI, is not
    // the new account's.
    let sessions = sessions(vec![
        record(OLD_ACCOUNT, "cli-1", 0, 2),
        record(NEW_ACCOUNT, "cli-1", 0, 20),
    ]);

    let new_account = claude_desktop_session_takeovers(&sessions)
        .into_iter()
        .find(|takeover| takeover.account_uuid == NEW_ACCOUNT)
        .expect("new account takes over");
    assert_eq!(new_account.active_from, Some(utc(at(9))));
    assert_eq!(new_account.certain_until, Some(utc(at(20))));
}
