#[cfg(test)]
use super::app::should_auto_install_background_tracking;
use super::{install_menu_tracking_observers, MenuTrackingObservers, UserEvent};
use macos::Activity;
use statsai::snapshot::{AppSnapshot, PrimaryAction, SnapshotBackgroundStatus};
use std::cell::{Cell, RefCell};
use std::process::Command;
use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
use tray_icon::TrayIcon;

mod macos {
    pub(super) use super::super::Activity;
}

pub(super) struct MenuShellContext<'a> {
    menu_shell: &'a RefCell<String>,
    pending_shell: &'a RefCell<Option<String>>,
    menu_tracking_observers: &'a RefCell<Option<MenuTrackingObservers>>,
    proxy: &'a tao::event_loop::EventLoopProxy<UserEvent>,
}

impl<'a> MenuShellContext<'a> {
    pub(super) fn new(
        menu_shell: &'a RefCell<String>,
        pending_shell: &'a RefCell<Option<String>>,
        menu_tracking_observers: &'a RefCell<Option<MenuTrackingObservers>>,
        proxy: &'a tao::event_loop::EventLoopProxy<UserEvent>,
    ) -> Self {
        Self {
            menu_shell,
            pending_shell,
            menu_tracking_observers,
            proxy,
        }
    }

    fn replace_tray_menu(&self, icon: Option<&TrayIcon>, menu: Menu) {
        let Some(icon) = icon else {
            return;
        };
        let observers = install_menu_tracking_observers(&menu, self.proxy.clone());
        icon.set_menu(Some(Box::new(menu)));
        self.menu_tracking_observers.replace(Some(observers));
    }
}

pub(super) struct MenuActions {
    pub(super) start_tracking: MenuItem,
    pub(super) scan_now: MenuItem,
    pub(super) link_dashboard: MenuItem,
    pub(super) upload_now: MenuItem,
    pub(super) open_dashboard: MenuItem,
    pub(super) add_source: MenuItem,
    pub(super) help: MenuItem,
    pub(super) quit: MenuItem,
}

pub(super) struct MenuUi {
    pub(super) summary: MenuItem,
    tracking: MenuItem,
    last_scan: MenuItem,
    stat_1: MenuItem,
    stat_2: MenuItem,
    stat_3: MenuItem,
    sources_header: MenuItem,
    pub(super) actions: MenuActions,
    #[cfg(debug_assertions)]
    dev_info: MenuItem,
}

impl MenuUi {
    pub(super) fn new() -> Self {
        Self {
            summary: status_item("Getting ready…"),
            tracking: status_item("Tracking · starting…"),
            last_scan: status_item("Last scan · …"),
            stat_1: status_item("Last 7 days · …"),
            stat_2: status_item("Today · …"),
            stat_3: status_item("Dashboard · …"),
            sources_header: status_item("Sources · all time"),
            actions: MenuActions {
                start_tracking: MenuItem::new("Start Local Tracking", true, None),
                scan_now: MenuItem::new("Scan Now", true, None),
                link_dashboard: MenuItem::new("Link Dashboard…", true, None),
                upload_now: MenuItem::new("Sync Now", true, None),
                open_dashboard: MenuItem::new("Open Dashboard", true, None),
                add_source: MenuItem::new("Add Source", true, None),
                help: MenuItem::new("Help", true, None),
                quit: MenuItem::new("Quit StatsAI", true, None),
            },
            #[cfg(debug_assertions)]
            dev_info: status_item(" "),
        }
    }

    pub(super) fn apply_snapshot(&self, snapshot: &AppSnapshot, activity: Activity) {
        let summary = match activity {
            Activity::Scanning => "Reading your usage logs…",
            Activity::Uploading => "Uploading to your dashboard…",
            Activity::Idle => snapshot.menu_summary.as_str(),
        };
        self.summary.set_text(summary);
        self.tracking
            .set_text(tracking_line(&snapshot.background_tracking));
        self.last_scan.set_text(
            snapshot
                .last_scan_summary
                .as_deref()
                .unwrap_or("Last scan · …"),
        );
        self.stat_1.set_text(&snapshot.menu_stat_1);
        self.stat_2.set_text(&snapshot.menu_stat_2);
        self.stat_3.set_text(&snapshot.menu_stat_3);

        let presentation = menu_presentation(snapshot, activity);
        self.actions
            .start_tracking
            .set_enabled(presentation.show_start_tracking);
        self.actions
            .scan_now
            .set_enabled(presentation.show_scan_now);
        self.actions
            .link_dashboard
            .set_enabled(presentation.show_link_dashboard);
        self.actions
            .upload_now
            .set_enabled(presentation.show_upload_now);
        self.actions
            .open_dashboard
            .set_enabled(presentation.open_dashboard_enabled);
        self.actions.add_source.set_enabled(true);
        self.actions.help.set_enabled(true);

        #[cfg(debug_assertions)]
        self.dev_info.set_text(dev_info_line(snapshot));
    }

    pub(super) fn build_menu(&self, snapshot: &AppSnapshot, activity: Activity) -> Menu {
        let menu = Menu::new();
        let sep_status = PredefinedMenuItem::separator();
        let sep_sources = PredefinedMenuItem::separator();
        let sep_actions = PredefinedMenuItem::separator();
        let sep_quit = PredefinedMenuItem::separator();
        #[cfg(debug_assertions)]
        let sep_dev = PredefinedMenuItem::separator();

        let presentation = menu_presentation(snapshot, activity);

        let _ = menu.append(&self.summary);
        let _ = menu.append(&self.tracking);
        let _ = menu.append(&self.last_scan);
        let _ = menu.append(&sep_status);
        let _ = menu.append(&self.stat_2);
        let _ = menu.append(&self.stat_1);
        let _ = menu.append(&self.stat_3);

        if snapshot.status_error {
            let _ = menu.append(&sep_actions);
            let _ = menu.append(&self.actions.help);
            let _ = menu.append(&sep_quit);
            let _ = menu.append(&self.actions.quit);
            return menu;
        }

        if presentation.show_sources {
            let _ = menu.append(&sep_sources);
            let _ = menu.append(&self.sources_header);
            for source in &snapshot.sources {
                let item = status_item(&source.label);
                let _ = menu.append(&item);
            }
        }

        let _ = menu.append(&sep_actions);
        if presentation.show_start_tracking {
            let _ = menu.append(&self.actions.start_tracking);
        }
        if presentation.show_link_dashboard {
            let _ = menu.append(&self.actions.link_dashboard);
        }
        if presentation.show_upload_now {
            let _ = menu.append(&self.actions.upload_now);
        }
        let _ = menu.append(&self.actions.open_dashboard);
        let _ = menu.append(&self.actions.scan_now);
        let _ = menu.append(&self.actions.add_source);
        let _ = menu.append(&self.actions.help);
        let _ = menu.append(&sep_quit);

        #[cfg(debug_assertions)]
        {
            let _ = menu.append(&self.dev_info);
            let _ = menu.append(&sep_dev);
        }

        let _ = menu.append(&self.actions.quit);
        menu
    }
}

pub(super) fn menu_shell_key(snapshot: &AppSnapshot, activity: Activity) -> String {
    if snapshot.status_error {
        return "error".to_string();
    }
    let sources = snapshot
        .sources
        .iter()
        .map(|source| {
            format!(
                "{}:{}:{}:{}",
                source.provider, source.status, source.token_count, source.label
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let presentation = menu_presentation(snapshot, activity);
    format!(
        "layout:{}:{activity:?}:first_run:{}:tracking:{}:actions:{}:{}:{}:{}:sources:{sources}",
        snapshot.menu_layout,
        snapshot.first_run,
        snapshot.background_tracking.running,
        presentation.show_start_tracking,
        presentation.show_scan_now,
        presentation.show_link_dashboard,
        presentation.show_upload_now,
    )
}

pub(super) fn sync_menu_shell(
    menu_ui: &MenuUi,
    icon: Option<&TrayIcon>,
    snapshot: &AppSnapshot,
    activity: Activity,
    menu_open: &Cell<bool>,
    context: &MenuShellContext<'_>,
) {
    let shell = menu_shell_key(snapshot, activity);
    let current_shell = context.menu_shell.borrow().clone();

    if shell == current_shell {
        menu_ui.apply_snapshot(snapshot, activity);
        context.pending_shell.borrow_mut().take();
        return;
    }

    if menu_open.get() {
        *context.pending_shell.borrow_mut() = Some(shell);
        menu_ui.apply_snapshot(snapshot, activity);
        return;
    }

    let menu = menu_ui.build_menu(snapshot, activity);
    context.replace_tray_menu(icon, menu);
    *context.menu_shell.borrow_mut() = shell;
    context.pending_shell.borrow_mut().take();
    menu_ui.apply_snapshot(snapshot, activity);
}

pub(super) fn flush_pending_menu_shell(
    menu_ui: &MenuUi,
    icon: Option<&TrayIcon>,
    snapshot: Option<AppSnapshot>,
    activity: Activity,
    context: &MenuShellContext<'_>,
) {
    let Some(shell) = context.pending_shell.borrow_mut().take() else {
        return;
    };
    if shell == *context.menu_shell.borrow() {
        return;
    }
    let Some(snapshot) = snapshot else {
        *context.pending_shell.borrow_mut() = Some(shell);
        return;
    };
    let menu = menu_ui.build_menu(&snapshot, activity);
    context.replace_tray_menu(icon, menu);
    *context.menu_shell.borrow_mut() = shell;
    menu_ui.apply_snapshot(&snapshot, activity);
}

pub(super) fn rebuild_menu_for_activity(
    menu_ui: &MenuUi,
    icon: Option<&TrayIcon>,
    snapshot: Option<AppSnapshot>,
    activity: Activity,
    context: &MenuShellContext<'_>,
) {
    let Some(snapshot) = snapshot else {
        return;
    };
    let shell = menu_shell_key(&snapshot, activity);
    let menu = menu_ui.build_menu(&snapshot, activity);
    context.replace_tray_menu(icon, menu);
    *context.menu_shell.borrow_mut() = shell;
    context.pending_shell.borrow_mut().take();
    menu_ui.apply_snapshot(&snapshot, activity);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MenuPresentation {
    show_sources: bool,
    show_start_tracking: bool,
    show_scan_now: bool,
    show_link_dashboard: bool,
    show_upload_now: bool,
    open_dashboard_enabled: bool,
}

fn menu_presentation(snapshot: &AppSnapshot, activity: Activity) -> MenuPresentation {
    let idle = activity == Activity::Idle && !snapshot.status_error;
    MenuPresentation {
        show_sources: !snapshot.sources.is_empty(),
        show_start_tracking: idle && !snapshot.background_tracking.running,
        show_scan_now: idle,
        show_link_dashboard: idle && !snapshot.logged_in,
        show_upload_now: idle
            && snapshot.logged_in
            && (snapshot.pending_upload || snapshot.sync_failures > 0),
        open_dashboard_enabled: !snapshot.status_error,
    }
}

fn tracking_line(status: &SnapshotBackgroundStatus) -> String {
    format!("Tracking · {}", status.label)
}

pub(super) fn should_run_startup_scan(snapshot: &AppSnapshot) -> bool {
    snapshot.first_run || snapshot.sessions_week == 0 || !snapshot.background_tracking.running
}

pub(super) fn loading_snapshot() -> AppSnapshot {
    AppSnapshot {
        logged_in: false,
        first_run: true,
        last_sync_at: None,
        sync_failures: 0,
        has_synced: false,
        pending_upload: false,
        pending_days: 0,
        unsynced_events: 0,
        tokens_today: 0,
        tokens_week: 0,
        sessions_week: 0,
        cost_week_cents: None,
        menu_summary: "Getting ready…".to_string(),
        menu_stat_1: "Last 7 days · …".to_string(),
        menu_stat_2: "Today · …".to_string(),
        menu_stat_3: "Dashboard · …".to_string(),
        primary_action: PrimaryAction::None,
        menu_layout: "loading".to_string(),
        status_error: false,
        backend_api: String::new(),
        backend_web: String::new(),
        using_local_dev: false,
        background_tracking: SnapshotBackgroundStatus {
            installed: false,
            running: false,
            label: "Tracking setup needed".to_string(),
        },
        sources: Vec::new(),
        last_scan_summary: Some("Last scan · waiting to start".to_string()),
        help_url: help_url(),
        setup_url: dashboard_url(),
        tooltip: "StatsAI".to_string(),
    }
}

pub(super) fn unavailable_snapshot(reason: &str) -> AppSnapshot {
    eprintln!("statsai menubar status error: {reason}");
    AppSnapshot {
        logged_in: false,
        first_run: false,
        last_sync_at: None,
        sync_failures: 0,
        has_synced: false,
        pending_upload: false,
        pending_days: 0,
        unsynced_events: 0,
        tokens_today: 0,
        tokens_week: 0,
        sessions_week: 0,
        cost_week_cents: None,
        menu_summary: "Can't check status right now".to_string(),
        menu_stat_1: "Try quitting and reopening StatsAI.".to_string(),
        menu_stat_2: "If macOS asked for Keychain access, click Allow.".to_string(),
        menu_stat_3: " ".to_string(),
        primary_action: PrimaryAction::None,
        menu_layout: "error".to_string(),
        status_error: true,
        backend_api: String::new(),
        backend_web: String::new(),
        using_local_dev: false,
        background_tracking: SnapshotBackgroundStatus {
            installed: false,
            running: false,
            label: "Tracking unavailable".to_string(),
        },
        sources: Vec::new(),
        last_scan_summary: Some("Last scan unavailable".to_string()),
        help_url: help_url(),
        setup_url: dashboard_url(),
        tooltip: "StatsAI — status unavailable".to_string(),
    }
}

fn status_item(label: &str) -> MenuItem {
    let item = MenuItem::new(label, false, None);
    item.set_enabled(false);
    item
}

pub(super) fn open_url(url: &str) {
    let _ = Command::new("open").arg(url).status();
}

pub(super) fn dashboard_url() -> String {
    let base =
        std::env::var("STATSAI_WEB_URL").unwrap_or_else(|_| "https://statsai.dev".to_string());
    format!("{}/dashboard/", base.trim_end_matches('/'))
}

pub(super) fn help_url() -> String {
    let base =
        std::env::var("STATSAI_WEB_URL").unwrap_or_else(|_| "https://statsai.dev".to_string());
    format!("{}/help/setup", base.trim_end_matches('/'))
}

#[cfg(debug_assertions)]
fn dev_info_line(snapshot: &AppSnapshot) -> String {
    if snapshot.using_local_dev {
        format!(
            "Local dev · {} · {}",
            shorten_url(&snapshot.backend_api),
            shorten_url(&snapshot.backend_web),
        )
    } else {
        "Developer build".to_string()
    }
}

#[cfg(debug_assertions)]
fn shorten_url(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_string()
}

pub(super) fn choose_source_folder(display_name: &str) -> Result<Option<String>, String> {
    let prompt = format!("Choose the {display_name} data folder to track.");
    let script = format!(
        "try\nPOSIX path of (choose folder with prompt {})\non error number -128\nreturn \"\"\nend try",
        applescript_string(&prompt)
    );
    let output = Command::new("osascript")
        .args(["-e", &script])
        .output()
        .map_err(|err| format!("failed to open folder picker: {err}"))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if message.is_empty() {
            return Err("folder picker failed".to_string());
        }
        return Err(message);
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        Ok(None)
    } else {
        Ok(Some(path))
    }
}

pub(super) fn choose_source_provider() -> Result<Option<(&'static str, &'static str)>, String> {
    let script = r#"try
set providerNames to {"Codex", "Claude Code", "OpenCode", "Grok Build"}
set chosenProvider to choose from list providerNames with prompt "Which source do you want to add?" without multiple selections allowed
if chosenProvider is false then return ""
return item 1 of chosenProvider
on error number -128
return ""
end try"#;
    let output = Command::new("osascript")
        .args(["-e", script])
        .output()
        .map_err(|err| format!("failed to open provider picker: {err}"))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if message.is_empty() {
            return Err("provider picker failed".to_string());
        }
        return Err(message);
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "" => Ok(None),
        "Codex" => Ok(Some(("codex", "Codex"))),
        "Claude Code" => Ok(Some(("claude_code", "Claude Code"))),
        "OpenCode" => Ok(Some(("opencode", "OpenCode"))),
        "Grok Build" => Ok(Some(("grok_build", "Grok Build"))),
        other => Err(format!("unknown source provider: {other}")),
    }
}

fn applescript_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub(super) fn alert(title: &str, message: &str) {
    let script = format!(
        "display alert {} message {} as informational",
        applescript_string(title),
        applescript_string(message),
    );
    let _ = Command::new("osascript").args(["-e", &script]).status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use statsai::snapshot::SnapshotSourceStatus;

    fn test_snapshot() -> AppSnapshot {
        AppSnapshot {
            logged_in: false,
            first_run: true,
            last_sync_at: None,
            sync_failures: 0,
            has_synced: false,
            pending_upload: false,
            pending_days: 0,
            unsynced_events: 0,
            tokens_today: 0,
            tokens_week: 0,
            sessions_week: 0,
            cost_week_cents: None,
            menu_summary: "StatsAI is tracking locally".to_string(),
            menu_stat_1: "Last 7 days · no requests yet".to_string(),
            menu_stat_2: "Today · no requests yet".to_string(),
            menu_stat_3: "Dashboard · not connected".to_string(),
            primary_action: PrimaryAction::Link,
            backend_api: "https://api.statsai.dev".to_string(),
            backend_web: "https://statsai.dev".to_string(),
            using_local_dev: false,
            background_tracking: SnapshotBackgroundStatus {
                installed: false,
                running: false,
                label: "Tracking setup needed".to_string(),
            },
            sources: vec![SnapshotSourceStatus {
                provider: "codex".to_string(),
                display_name: "Codex".to_string(),
                configured: false,
                discovered: true,
                enabled: true,
                has_data: false,
                event_count: 0,
                token_count: 0,
                estimated_cost_cents: None,
                label: "Codex · 0 tokens · $0".to_string(),
                status: "found".to_string(),
            }],
            last_scan_summary: Some("Last scan found no requests yet".to_string()),
            help_url: "https://statsai.dev/help/setup".to_string(),
            setup_url: "https://statsai.dev/dashboard/".to_string(),
            tooltip: "StatsAI".to_string(),
            menu_layout: "unlinked".to_string(),
            status_error: false,
        }
    }

    #[test]
    fn first_run_menu_exposes_setup_scan_and_link_actions() {
        let snapshot = test_snapshot();
        let presentation = menu_presentation(&snapshot, Activity::Idle);

        assert!(presentation.show_sources);
        assert!(presentation.show_start_tracking);
        assert!(presentation.show_scan_now);
        assert!(presentation.show_link_dashboard);
        assert!(!presentation.show_upload_now);
        assert!(presentation.open_dashboard_enabled);
    }

    #[test]
    fn pending_upload_menu_hides_login_and_shows_upload() {
        let mut snapshot = test_snapshot();
        snapshot.logged_in = true;
        snapshot.first_run = false;
        snapshot.pending_upload = true;
        snapshot.background_tracking.running = true;

        let presentation = menu_presentation(&snapshot, Activity::Idle);

        assert!(!presentation.show_start_tracking);
        assert!(presentation.show_scan_now);
        assert!(!presentation.show_link_dashboard);
        assert!(presentation.show_upload_now);
    }

    #[test]
    fn busy_and_error_states_disable_mutating_menu_actions() {
        let mut snapshot = test_snapshot();
        let scanning = menu_presentation(&snapshot, Activity::Scanning);
        assert!(!scanning.show_scan_now);
        assert!(!scanning.show_link_dashboard);
        assert!(!scanning.show_upload_now);

        snapshot.status_error = true;
        let error = menu_presentation(&snapshot, Activity::Idle);
        assert!(!error.show_scan_now);
        assert!(!error.show_start_tracking);
        assert!(!error.open_dashboard_enabled);
    }

    #[test]
    fn tracking_line_uses_snapshot_label() {
        let snapshot = test_snapshot();
        assert_eq!(
            tracking_line(&snapshot.background_tracking),
            "Tracking · Tracking setup needed"
        );
    }

    #[test]
    fn startup_scan_only_runs_for_setup_or_empty_states() {
        let first_run = test_snapshot();
        assert!(should_run_startup_scan(&first_run));

        let mut ready = test_snapshot();
        ready.first_run = false;
        ready.sessions_week = 10;
        ready.background_tracking.running = true;
        assert!(!should_run_startup_scan(&ready));
    }

    #[test]
    fn debug_build_does_not_auto_install_persistent_tracking() {
        assert!(!should_auto_install_background_tracking(true));
        assert!(should_auto_install_background_tracking(false));
    }
}
