use std::process::Command;

fn run_statsai(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_statsai"))
        .args(args)
        .env("COLUMNS", "80")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .env("TERM", "dumb")
        .env("STATSAI_DEVICE_ID", "cli-surface-test")
        .output()
        .expect("run statsai");
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf-8 stdout")
}

fn parse_commands(help: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut in_commands = false;
    for line in help.lines() {
        if line == "Commands:" {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some(name) = command_name(line) {
            if name != "help" {
                commands.push(name.to_string());
            }
            continue;
        }
        if line.starts_with("    ") {
            continue;
        }
        break;
    }
    commands
}

fn command_name(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("  ")?;
    if rest.starts_with(' ') {
        return None;
    }
    let name = rest.split_whitespace().next()?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !name.starts_with(|ch: char| ch.is_ascii_lowercase())
    {
        return None;
    }
    if rest == name || rest[name.len()..].starts_with("  ") {
        Some(name)
    } else {
        None
    }
}

fn heading(prefix: &[String]) -> String {
    if prefix.is_empty() {
        "===== statsai --help =====".to_string()
    } else {
        format!("===== statsai {} --help =====", prefix.join(" "))
    }
}

fn walk_help(prefix: &[String], out: &mut String) {
    let mut args: Vec<String> = prefix.to_vec();
    args.push("--help".to_string());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let help = run_statsai(&arg_refs);
    out.push_str(&heading(prefix));
    out.push('\n');
    out.push_str(&help);
    if !help.ends_with('\n') {
        out.push('\n');
    }
    for command in parse_commands(&help) {
        let mut next = prefix.to_vec();
        next.push(command);
        walk_help(&next, out);
    }
}

#[test]
fn cli_help_tree_matches_golden() {
    let mut captured = String::new();
    walk_help(&[], &mut captured);
    let golden = include_str!("cli_surface/help.txt");
    assert_eq!(captured, golden);
}

#[test]
fn schema_sync_batch_matches_golden() {
    let captured = run_statsai(&["schema", "sync-batch"]);
    let golden = include_str!("cli_surface/schema-sync-batch.json");
    assert_eq!(captured, golden);
}

#[test]
fn schema_quota_window_projection_matches_golden() {
    let captured = run_statsai(&["schema", "quota-window-projection"]);
    let golden = include_str!("cli_surface/schema-quota-window-projection.json");
    assert_eq!(captured, golden);
}
