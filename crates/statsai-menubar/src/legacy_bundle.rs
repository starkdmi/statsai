//! Decisions for retiring the menu bar app's old bundle name.
//!
//! Releases up to 0.7.2 shipped the menu bar app as `StatsAI.app`, a name it
//! now shares with an unrelated app. The renamed bundle keeps the identifier,
//! so at launch it moves an old copy with that identifier to the Trash and
//! points the user's login item at itself. The macOS side effects live in
//! `macos::legacy`; everything here is plain logic so it is tested on every
//! platform.

use std::path::{Path, PathBuf};

/// The identifier both bundle names share. Only a bundle with exactly this
/// identifier is ever treated as an old copy of the menu bar app.
pub(crate) const BUNDLE_IDENTIFIER: &str = "dev.statsai.app";

/// The bundle name releases up to 0.7.2 installed.
pub(crate) const LEGACY_BUNDLE_NAME: &str = "StatsAI.app";

/// Lists every login item as `hidden<TAB>path` lines. Items without a path are
/// skipped rather than failing the whole list.
pub(crate) const LIST_LOGIN_ITEMS_SCRIPT: &str = r#"set output to ""
tell application "System Events"
    repeat with itemRef in login items
        try
            set output to output & ((hidden of itemRef) as text) & tab & (path of itemRef) & linefeed
        end try
    end repeat
end tell
return output"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoginItem {
    pub(crate) path: String,
    pub(crate) hidden: bool,
}

/// Where an old bundle may be installed: `/Applications` and `~/Applications`.
pub(crate) fn legacy_bundle_candidates(home: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = vec![Path::new("/Applications").join(LEGACY_BUNDLE_NAME)];
    if let Some(home) = home.filter(|home| home.is_absolute()) {
        candidates.push(home.join("Applications").join(LEGACY_BUNDLE_NAME));
    }
    candidates
}

/// The `.app` directory an executable at `X.app/Contents/MacOS/<name>` runs
/// from, or `None` when it does not run from a bundle (for example `cargo run`).
pub(crate) fn running_bundle(exe: &Path) -> Option<PathBuf> {
    let contents = exe.parent()?.parent()?;
    if contents.file_name()? != "Contents" || exe.parent()?.file_name()? != "MacOS" {
        return None;
    }
    let bundle = contents.parent()?;
    (bundle.extension()? == "app").then(|| bundle.to_path_buf())
}

/// Gatekeeper runs a quarantined app from a randomized read-only copy. A login
/// item pointing there would break, so nothing is migrated from such a copy.
pub(crate) fn is_translocated(bundle: &Path) -> bool {
    bundle
        .components()
        .any(|component| component.as_os_str() == "AppTranslocation")
}

pub(crate) fn is_menu_bar_identifier(identifier: &str) -> bool {
    identifier.trim() == BUNDLE_IDENTIFIER
}

/// Parses the output of [`LIST_LOGIN_ITEMS_SCRIPT`].
pub(crate) fn parse_login_items(output: &str) -> Vec<LoginItem> {
    output
        .lines()
        .filter_map(|line| {
            let (hidden, path) = line.split_once('\t')?;
            let path = path.trim_end_matches('\r');
            (!path.is_empty()).then(|| LoginItem {
                path: path.to_string(),
                hidden: hidden.trim() == "true",
            })
        })
        .collect()
}

pub(crate) fn login_item_points_at(item_path: &str, bundle: &Path) -> bool {
    let Some(bundle) = bundle.to_str() else {
        return false;
    };
    let item_path = item_path.trim_end_matches('/');
    !item_path.is_empty() && item_path == bundle.trim_end_matches('/')
}

/// The System Events script that replaces login items for `legacy` with one
/// for `current`, keeping the old item's visibility. `None` when no login item
/// points at `legacy`, so no login item is ever created from nothing, or when a
/// path cannot be written as an AppleScript string.
pub(crate) fn login_item_update_script(
    items: &[LoginItem],
    legacy: &Path,
    current: &Path,
) -> Option<String> {
    let stale = items
        .iter()
        .filter(|item| login_item_points_at(&item.path, legacy))
        .collect::<Vec<_>>();
    if stale.is_empty() {
        return None;
    }

    let mut script = String::from("tell application \"System Events\"\n");
    let mut deleted = Vec::new();
    for item in &stale {
        if deleted.contains(&item.path.as_str()) {
            continue;
        }
        deleted.push(item.path.as_str());
        script.push_str(&format!(
            "    delete (every login item whose path is {})\n",
            applescript_string(&item.path)?
        ));
    }
    if !items
        .iter()
        .any(|item| login_item_points_at(&item.path, current))
    {
        let hidden = stale.iter().any(|item| item.hidden);
        script.push_str(&format!(
            "    make login item at end with properties {{path:{}, hidden:{hidden}}}\n",
            applescript_string(current.to_str()?)?
        ));
    }
    script.push_str("end tell");
    Some(script)
}

/// Quotes `value` as an AppleScript string literal. Control characters are
/// refused rather than escaped: no real bundle path contains them.
pub(crate) fn applescript_string(value: &str) -> Option<String> {
    if value.chars().any(char::is_control) {
        return None;
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for ch in value.chars() {
        if matches!(ch, '"' | '\\') {
            quoted.push('\\');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    Some(quoted)
}

/// Whether a LaunchAgent plist written by `statsai service install` runs a
/// program from inside `bundle`. Paths in the plist are XML-escaped.
pub(crate) fn launch_agent_points_into(plist: &str, bundle: &Path) -> bool {
    let Some(bundle) = bundle.to_str() else {
        return false;
    };
    let prefix = format!(
        "<string>{}/Contents/",
        xml_escape(bundle.trim_end_matches('/'))
    );
    plist.contains(&prefix)
}

/// The alert title and message for old copies left in place because their
/// login item could not be moved, or `None` when every old copy was retired.
/// Such a copy is still running and starts at every login, holding the lock
/// this app needs, so the user has to remove it by hand.
pub(crate) fn unretired_bundles_alert(bundles: &[PathBuf]) -> Option<(String, String)> {
    let paths = bundles
        .iter()
        .map(|bundle| bundle.display().to_string())
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return None;
    }
    let message = format!(
        "StatsAI Menu Bar could not move the login item of the older copy at {}, so that copy \
         is still installed and opens at login instead of this one. To finish the update, \
         remove StatsAI from System Settings > General > Login Items, quit it, move {} to the \
         Trash, then open StatsAI Menu Bar again.",
        paths.join(" and "),
        if paths.len() == 1 { "it" } else { "them" },
    );
    Some((
        "StatsAI Menu Bar could not replace the old StatsAI.app".to_string(),
        message,
    ))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, hidden: bool) -> LoginItem {
        LoginItem {
            path: path.to_string(),
            hidden,
        }
    }

    #[test]
    fn candidates_cover_system_and_user_applications() {
        assert_eq!(
            legacy_bundle_candidates(Some(Path::new("/Users/me"))),
            vec![
                PathBuf::from("/Applications/StatsAI.app"),
                PathBuf::from("/Users/me/Applications/StatsAI.app"),
            ]
        );
        assert_eq!(
            legacy_bundle_candidates(None),
            vec![PathBuf::from("/Applications/StatsAI.app")]
        );
        assert_eq!(
            legacy_bundle_candidates(Some(Path::new("relative"))),
            vec![PathBuf::from("/Applications/StatsAI.app")]
        );
    }

    #[test]
    fn running_bundle_requires_the_app_layout() {
        assert_eq!(
            running_bundle(Path::new(
                "/Applications/StatsAIMenuBar.app/Contents/MacOS/statsai-menubar"
            )),
            Some(PathBuf::from("/Applications/StatsAIMenuBar.app"))
        );
        assert_eq!(
            running_bundle(Path::new("/repo/target/debug/statsai-menubar")),
            None
        );
        assert_eq!(
            running_bundle(Path::new(
                "/Applications/Thing/Contents/MacOS/statsai-menubar"
            )),
            None
        );
        assert_eq!(
            running_bundle(Path::new(
                "/Applications/X.app/Contents/Resources/statsai-menubar"
            )),
            None
        );
        assert_eq!(running_bundle(Path::new("statsai-menubar")), None);
    }

    #[test]
    fn translocated_bundles_are_detected() {
        assert!(is_translocated(Path::new(
            "/private/var/folders/ab/T/AppTranslocation/0000-1111/d/StatsAIMenuBar.app"
        )));
        assert!(!is_translocated(Path::new(
            "/Applications/StatsAIMenuBar.app"
        )));
    }

    #[test]
    fn only_the_menu_bar_identifier_is_migrated() {
        assert!(is_menu_bar_identifier("dev.statsai.app"));
        assert!(is_menu_bar_identifier("dev.statsai.app\n"));
        assert!(!is_menu_bar_identifier("dev.statsai.desktop"));
        assert!(!is_menu_bar_identifier("dev.statsai.app.helper"));
        assert!(!is_menu_bar_identifier("DEV.STATSAI.APP"));
        assert!(!is_menu_bar_identifier(""));
    }

    #[test]
    fn login_items_parse_from_tab_separated_lines() {
        let output = "false\t/Applications/StatsAI.app\ntrue\t/Applications/Mail Helper.app\r\n\nfalse\t\nmalformed\n";
        assert_eq!(
            parse_login_items(output),
            vec![
                item("/Applications/StatsAI.app", false),
                item("/Applications/Mail Helper.app", true),
            ]
        );
    }

    #[test]
    fn login_item_paths_match_with_or_without_trailing_slash() {
        let legacy = Path::new("/Applications/StatsAI.app");
        assert!(login_item_points_at("/Applications/StatsAI.app", legacy));
        assert!(login_item_points_at("/Applications/StatsAI.app/", legacy));
        assert!(!login_item_points_at(
            "/Applications/StatsAI.app/Contents/MacOS/statsai-menubar",
            legacy
        ));
        assert!(!login_item_points_at("/Applications/StatsAI.apps", legacy));
        assert!(!login_item_points_at(
            "/Users/me/Applications/StatsAI.app",
            legacy
        ));
        assert!(!login_item_points_at("", Path::new("/")));
    }

    #[test]
    fn login_item_moves_to_the_new_bundle_and_keeps_visibility() {
        let script = login_item_update_script(
            &[
                item("/Applications/Other.app", false),
                item("/Applications/StatsAI.app", true),
            ],
            Path::new("/Applications/StatsAI.app"),
            Path::new("/Applications/StatsAIMenuBar.app"),
        )
        .expect("a login item points at the old bundle");
        assert_eq!(
            script,
            "tell application \"System Events\"\n    \
             delete (every login item whose path is \"/Applications/StatsAI.app\")\n    \
             make login item at end with properties {path:\"/Applications/StatsAIMenuBar.app\", hidden:true}\n\
             end tell"
        );
    }

    #[test]
    fn no_login_item_is_created_when_none_pointed_at_the_old_bundle() {
        assert_eq!(
            login_item_update_script(
                &[item("/Applications/Other.app", false)],
                Path::new("/Applications/StatsAI.app"),
                Path::new("/Applications/StatsAIMenuBar.app"),
            ),
            None
        );
        assert_eq!(
            login_item_update_script(
                &[],
                Path::new("/Applications/StatsAI.app"),
                Path::new("/Applications/StatsAIMenuBar.app"),
            ),
            None
        );
    }

    #[test]
    fn existing_login_item_for_the_new_bundle_is_not_duplicated() {
        let script = login_item_update_script(
            &[
                item("/Applications/StatsAI.app", false),
                item("/Applications/StatsAI.app/", false),
                item("/Applications/StatsAIMenuBar.app", false),
            ],
            Path::new("/Applications/StatsAI.app"),
            Path::new("/Applications/StatsAIMenuBar.app"),
        )
        .expect("login items point at the old bundle");
        assert_eq!(script.matches("delete").count(), 2);
        assert!(!script.contains("make login item"));
    }

    #[test]
    fn login_item_paths_are_escaped_for_applescript() {
        let script = login_item_update_script(
            &[item("/Users/a \"b\"/Applications/StatsAI.app", false)],
            Path::new("/Users/a \"b\"/Applications/StatsAI.app"),
            Path::new("/Users/a\\b/StatsAIMenuBar.app"),
        )
        .expect("a login item points at the old bundle");
        assert!(script.contains(r#"whose path is "/Users/a \"b\"/Applications/StatsAI.app""#));
        assert!(script.contains(r#"{path:"/Users/a\\b/StatsAIMenuBar.app", hidden:false}"#));
    }

    #[test]
    fn applescript_strings_refuse_control_characters() {
        assert_eq!(
            applescript_string(r#"a"b\c"#).as_deref(),
            Some(r#""a\"b\\c""#)
        );
        assert_eq!(applescript_string("line\nbreak"), None);
        assert_eq!(applescript_string("tab\there"), None);
        assert_eq!(
            login_item_update_script(
                &[item("/Applications/StatsAI.app", false)],
                Path::new("/Applications/StatsAI.app"),
                Path::new("/Applications/New\nLine.app"),
            ),
            None
        );
    }

    #[test]
    fn launch_agent_matches_programs_inside_the_bundle() {
        let plist = "<array>\n        <string>/Applications/StatsAI.app/Contents/MacOS/statsai</string>\n        <string>daemon</string>\n</array>";
        assert!(launch_agent_points_into(
            plist,
            Path::new("/Applications/StatsAI.app")
        ));
        assert!(!launch_agent_points_into(
            plist,
            Path::new("/Applications/StatsAIMenuBar.app")
        ));
        assert!(!launch_agent_points_into(
            "<string>/Applications/StatsAI.app.bak/Contents/MacOS/statsai</string>",
            Path::new("/Applications/StatsAI.app")
        ));
        assert!(launch_agent_points_into(
            "<string>/Users/a&amp;b/Applications/StatsAI.app/Contents/MacOS/statsai</string>",
            Path::new("/Users/a&b/Applications/StatsAI.app")
        ));
    }

    #[test]
    fn unretired_copies_get_an_alert_that_says_how_to_finish() {
        assert_eq!(unretired_bundles_alert(&[]), None);

        let (title, message) =
            unretired_bundles_alert(&[PathBuf::from("/Applications/StatsAI.app")])
                .expect("alert for one copy");
        assert_eq!(
            title,
            "StatsAI Menu Bar could not replace the old StatsAI.app"
        );
        assert!(message.contains("older copy at /Applications/StatsAI.app,"));
        assert!(message.contains("System Settings > General > Login Items"));
        assert!(message.contains("move it to the Trash"));
        assert!(message.contains("open StatsAI Menu Bar again"));

        let (_, message) = unretired_bundles_alert(&[
            PathBuf::from("/Applications/StatsAI.app"),
            PathBuf::from("/Users/me/Applications/StatsAI.app"),
        ])
        .expect("alert for two copies");
        assert!(
            message.contains("/Applications/StatsAI.app and /Users/me/Applications/StatsAI.app")
        );
        assert!(message.contains("move them to the Trash"));
    }
}
