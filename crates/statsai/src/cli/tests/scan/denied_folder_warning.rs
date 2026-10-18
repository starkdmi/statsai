use super::*;

// STA-40: tools that run scans parse this line, so its format is a contract.
#[test]
fn denied_folder_warning_line_is_one_line_with_a_json_folder() {
    let home = Some(Path::new("/Users/alice"));
    let cases = [
        (
            "/Users/alice/Documents",
            home,
            r#"scan warning: project_settings_denied provider=claude_code folder="~/Documents""#,
        ),
        (
            "/Volumes/Work",
            home,
            r#"scan warning: project_settings_denied provider=claude_code folder="/Volumes/Work""#,
        ),
        (
            "/Users/alice/Documents/My \"Repos\"/Café",
            home,
            r#"scan warning: project_settings_denied provider=claude_code folder="~/Documents/My \"Repos\"/Café""#,
        ),
        (
            "/Users/alice/Downloads/a\nb",
            home,
            r#"scan warning: project_settings_denied provider=claude_code folder="~/Downloads/a\nb""#,
        ),
        (
            "/Users/alice/Desktop",
            None,
            r#"scan warning: project_settings_denied provider=claude_code folder="/Users/alice/Desktop""#,
        ),
    ];
    for (folder, home, expected) in cases {
        let line = denied_folder_warning_line(Path::new(folder), home);
        assert_eq!(line, expected, "{folder:?}");
        assert!(!line.contains('\n'), "{folder:?} stays on one line");
        let json = line
            .split_once(" folder=")
            .map(|(_, json)| json)
            .expect("folder field");
        let parsed: String = serde_json::from_str(json).expect("folder is a JSON string");
        let expected_label = match home {
            Some(home) => statsai_core::collapse_home_path_label(folder, home),
            None => folder.to_string(),
        };
        assert_eq!(parsed, expected_label, "{folder:?} round-trips");
    }
}
