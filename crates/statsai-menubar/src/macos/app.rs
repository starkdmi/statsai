use super::capture::{fetch_snapshot, run_statsai_capture, run_statsai_capture_dynamic};
use super::menu::{
    alert, choose_source_folder, choose_source_provider, dashboard_url, flush_pending_menu_shell,
    help_url, loading_snapshot, menu_shell_key, open_url, rebuild_menu_for_activity,
    should_run_startup_scan, sync_menu_shell, unavailable_snapshot, MenuShellContext, MenuUi,
};
use super::{install_menu_tracking_observers, Activity, InstanceLock, UserEvent, REFRESH_INTERVAL};
use statsai::snapshot::AppSnapshot;
use std::cell::{Cell, RefCell};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::MenuEvent;
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub fn run() -> Result<(), String> {
    let _instance_lock = InstanceLock::acquire()?;

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = proxy.send_event(UserEvent::TrayIcon(event));
    }));
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = proxy.send_event(UserEvent::Menu(event));
    }));

    let refresh_proxy = event_loop.create_proxy();
    let init_refresh_proxy = refresh_proxy.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(REFRESH_INTERVAL);
        let _ = refresh_proxy.send_event(UserEvent::Refresh);
    });

    if should_auto_install_background_tracking(cfg!(debug_assertions)) {
        ensure_background_tracking();
    }

    let menu_ui = MenuUi::new();
    let initial = loading_snapshot();
    let initial_shell = menu_shell_key(&initial, Activity::Idle);
    menu_ui.apply_snapshot(&initial, Activity::Idle);
    let tray_menu = menu_ui.build_menu(&initial, Activity::Idle);
    let menu_tracking_observers = RefCell::new(Some(install_menu_tracking_observers(
        &tray_menu,
        init_refresh_proxy.clone(),
    )));
    let activity = Cell::new(Activity::Idle);
    let refresh_in_flight = Cell::new(false);
    let last_snapshot = RefCell::new(None::<AppSnapshot>);
    let startup_scan_started = Cell::new(false);
    let menu_open = Cell::new(false);
    let menu_shell = RefCell::new(initial_shell);
    let pending_shell = RefCell::new(None::<String>);

    let mut tray_icon: Option<TrayIcon> = None;
    let mut next_wakeup = std::time::Instant::now()
        .checked_add(REFRESH_INTERVAL)
        .unwrap_or_else(std::time::Instant::now);

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(next_wakeup);
        let menu_shell_context = MenuShellContext::new(
            &menu_shell,
            &pending_shell,
            &menu_tracking_observers,
            &init_refresh_proxy,
        );

        match event {
            Event::NewEvents(tao::event::StartCause::Init) if tray_icon.is_none() => {
                match TrayIconBuilder::new()
                    .with_menu(Box::new(tray_menu.clone()))
                    .with_tooltip("StatsAI")
                    .with_icon(tray_icon_image())
                    .build()
                {
                    Ok(icon) => {
                        tray_icon = Some(icon);

                        use objc2_core_foundation::CFRunLoop;
                        if let Some(rl) = CFRunLoop::main() {
                            CFRunLoop::wake_up(&rl);
                        }

                        let _ = init_refresh_proxy.send_event(UserEvent::Refresh);
                    }
                    Err(err) => {
                        eprintln!("StatsAI could not create the menu bar icon: {err}");
                        std::process::exit(1);
                    }
                }
            }
            Event::NewEvents(_) => {}
            Event::UserEvent(UserEvent::TrayIcon(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Down,
                ..
            })) => {
                menu_open.set(true);
            }
            Event::UserEvent(UserEvent::MenuTrackingChanged(is_open)) => {
                menu_open.set(is_open);
                if !is_open {
                    flush_pending_menu_shell(
                        &menu_ui,
                        tray_icon.as_ref(),
                        last_snapshot.borrow().clone(),
                        activity.get(),
                        &menu_shell_context,
                    );
                }
            }
            Event::UserEvent(UserEvent::SetIdle) => {
                activity.set(Activity::Idle);
                let _ = init_refresh_proxy.send_event(UserEvent::Refresh);
            }
            Event::UserEvent(UserEvent::Refresh) => {
                next_wakeup = std::time::Instant::now()
                    .checked_add(REFRESH_INTERVAL)
                    .unwrap_or_else(std::time::Instant::now);
                if refresh_in_flight.get() {
                    return;
                }
                refresh_in_flight.set(true);
                let proxy = init_refresh_proxy.clone();
                std::thread::spawn(move || {
                    let snapshot = fetch_snapshot();
                    let _ = proxy.send_event(UserEvent::SnapshotReady(Box::new(snapshot)));
                });
            }
            Event::UserEvent(UserEvent::SnapshotReady(result)) => {
                refresh_in_flight.set(false);
                let snapshot = match *result {
                    Ok(snapshot) => {
                        *last_snapshot.borrow_mut() = Some(snapshot.clone());
                        snapshot
                    }
                    Err(reason) => last_snapshot
                        .borrow()
                        .clone()
                        .unwrap_or_else(|| unavailable_snapshot(&reason)),
                };
                sync_menu_shell(
                    &menu_ui,
                    tray_icon.as_ref(),
                    &snapshot,
                    activity.get(),
                    &menu_open,
                    &menu_shell_context,
                );
                if let Some(icon) = tray_icon.as_ref() {
                    let _ = icon.set_tooltip(Some(snapshot.tooltip.as_str()));
                }
                if !startup_scan_started.get() && should_run_startup_scan(&snapshot) {
                    startup_scan_started.set(true);
                    activity.set(Activity::Scanning);
                    rebuild_menu_for_activity(
                        &menu_ui,
                        tray_icon.as_ref(),
                        Some(snapshot),
                        activity.get(),
                        &menu_shell_context,
                    );
                    spawn_startup_scan(init_refresh_proxy.clone(), init_refresh_proxy.clone());
                } else {
                    startup_scan_started.set(true);
                }
            }
            Event::UserEvent(UserEvent::Menu(menu_event)) => {
                if menu_event.id == menu_ui.actions.quit.id() {
                    menu_open.set(false);
                    tray_icon.take();
                    *control_flow = ControlFlow::Exit;
                    return;
                }
                if menu_event.id == menu_ui.actions.open_dashboard.id() {
                    menu_open.set(false);
                    open_url(&dashboard_url());
                    flush_pending_menu_shell(
                        &menu_ui,
                        tray_icon.as_ref(),
                        last_snapshot.borrow().clone(),
                        activity.get(),
                        &menu_shell_context,
                    );
                    return;
                }

                if menu_event.id == menu_ui.actions.help.id() {
                    menu_open.set(false);
                    let url = last_snapshot
                        .borrow()
                        .as_ref()
                        .map(|snapshot| snapshot.help_url.clone())
                        .filter(|url| !url.trim().is_empty())
                        .unwrap_or_else(help_url);
                    open_url(&url);
                    flush_pending_menu_shell(
                        &menu_ui,
                        tray_icon.as_ref(),
                        last_snapshot.borrow().clone(),
                        activity.get(),
                        &menu_shell_context,
                    );
                    return;
                }

                if menu_event.id == menu_ui.actions.start_tracking.id() {
                    let proxy = init_refresh_proxy.clone();
                    menu_ui.summary.set_text("Starting local tracking…");
                    spawn_menu_action(move || {
                        match run_statsai_capture(&["service", "install"]) {
                            Ok(_) => {
                                let _ = proxy.send_event(UserEvent::Refresh);
                            }
                            Err(message) => alert("Could not start local tracking", &message),
                        }
                        let _ = proxy.send_event(UserEvent::Refresh);
                    });
                }

                if menu_event.id == menu_ui.actions.add_source.id() {
                    spawn_add_source_action(init_refresh_proxy.clone());
                }

                if menu_event.id == menu_ui.actions.scan_now.id() {
                    let proxy = init_refresh_proxy.clone();
                    activity.set(Activity::Scanning);
                    menu_ui.summary.set_text("Reading your usage logs…");
                    rebuild_menu_for_activity(
                        &menu_ui,
                        tray_icon.as_ref(),
                        last_snapshot.borrow().clone(),
                        activity.get(),
                        &menu_shell_context,
                    );
                    spawn_scan_action(proxy);
                }

                if menu_event.id == menu_ui.actions.link_dashboard.id() {
                    let proxy = init_refresh_proxy.clone();
                    spawn_menu_action(move || {
                        match run_statsai_capture(&["auth", "login"]) {
                            Ok(_) => {
                                let _ = proxy.send_event(UserEvent::Refresh);
                            }
                            Err(message) => alert("Could not link this Mac", &message),
                        }
                        let _ = proxy.send_event(UserEvent::Refresh);
                    });
                }

                if menu_event.id == menu_ui.actions.upload_now.id() {
                    let proxy = init_refresh_proxy.clone();
                    activity.set(Activity::Uploading);
                    menu_ui.summary.set_text("Uploading to your dashboard…");
                    menu_ui.actions.upload_now.set_enabled(false);
                    rebuild_menu_for_activity(
                        &menu_ui,
                        tray_icon.as_ref(),
                        last_snapshot.borrow().clone(),
                        activity.get(),
                        &menu_shell_context,
                    );
                    spawn_upload_action(proxy);
                }
                menu_open.set(false);
                flush_pending_menu_shell(
                    &menu_ui,
                    tray_icon.as_ref(),
                    last_snapshot.borrow().clone(),
                    activity.get(),
                    &menu_shell_context,
                );
            }
            _ => {}
        }
    });
}

fn ensure_background_tracking() {
    spawn_menu_action(|| {
        if let Err(message) = run_statsai_capture(&["service", "install"]) {
            eprintln!("statsai menubar could not start background tracking: {message}");
        }
    });
}

pub(super) fn should_auto_install_background_tracking(debug_build: bool) -> bool {
    !debug_build
}

fn spawn_startup_scan(
    refresh_proxy: tao::event_loop::EventLoopProxy<UserEvent>,
    idle_proxy: tao::event_loop::EventLoopProxy<UserEvent>,
) {
    spawn_menu_action(move || {
        match run_statsai_capture(&["scan"]) {
            Ok(_) => {
                let _ = refresh_proxy.send_event(UserEvent::Refresh);
            }
            Err(message) => {
                eprintln!("statsai menubar startup scan failed: {message}");
            }
        }
        let _ = idle_proxy.send_event(UserEvent::SetIdle);
    });
}

fn spawn_scan_action(proxy: tao::event_loop::EventLoopProxy<UserEvent>) {
    spawn_menu_action(move || {
        match run_statsai_capture(&["scan"]) {
            Ok(_) => {}
            Err(message) => alert("Could not read your usage logs", &message),
        }
        let _ = proxy.send_event(UserEvent::SetIdle);
    });
}

fn spawn_upload_action(proxy: tao::event_loop::EventLoopProxy<UserEvent>) {
    spawn_menu_action(move || {
        match run_statsai_capture(&["scan"]) {
            Ok(_) => match run_statsai_capture(&["sync", "--sink", "http", "--since-last"]) {
                Ok(_) => {}
                Err(message) => alert("Upload failed", &message),
            },
            Err(message) => alert("Could not read your usage logs", &message),
        }
        let _ = proxy.send_event(UserEvent::SetIdle);
    });
}

fn spawn_add_source_action(proxy: tao::event_loop::EventLoopProxy<UserEvent>) {
    spawn_menu_action(move || {
        match choose_source_provider().and_then(|choice| match choice {
            Some((provider, display_name)) => {
                choose_source_folder(display_name).map(|path| path.map(|path| (provider, path)))
            }
            None => Ok(None),
        }) {
            Ok(Some((provider, path))) => {
                let args = vec![
                    "source".to_string(),
                    "add".to_string(),
                    "--provider".to_string(),
                    provider.to_string(),
                    "--path".to_string(),
                    path,
                ];
                match run_statsai_capture_dynamic(args) {
                    Ok(_) => {
                        let _ = run_statsai_capture(&["scan"]);
                        let _ = proxy.send_event(UserEvent::Refresh);
                    }
                    Err(message) => alert("Could not add source", &message),
                }
            }
            Ok(None) => {}
            Err(message) => alert("Could not choose folder", &message),
        }
        let _ = proxy.send_event(UserEvent::Refresh);
    });
}

fn spawn_menu_action(action: impl FnOnce() + Send + 'static) {
    std::thread::spawn(action);
}

fn tray_icon_image() -> tray_icon::Icon {
    load_tray_icon_from_png(include_bytes!("../../assets/icon.png")).unwrap_or_else(|err| {
        eprintln!("statsai menubar: could not load bundled icon: {err}");
        fallback_tray_icon()
    })
}

fn load_tray_icon_from_png(bytes: &[u8]) -> Result<tray_icon::Icon, String> {
    let decoder = png::Decoder::new(bytes);
    let mut reader = decoder
        .read_info()
        .map_err(|err| format!("decode tray icon png: {err}"))?;
    let mut rgba = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut rgba)
        .map_err(|err| format!("read tray icon png: {err}"))?;
    if info.color_type != png::ColorType::Rgba {
        return Err(format!(
            "expected RGBA tray icon, got {:?}",
            info.color_type
        ));
    }
    tray_icon::Icon::from_rgba(rgba, info.width, info.height)
        .map_err(|err| format!("build tray icon: {err}"))
}

fn fallback_tray_icon() -> tray_icon::Icon {
    let size = 22u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for y in 4..18 {
        for x in 5..17 {
            let i = ((y * size + x) * 4) as usize;
            rgba[i] = 24;
            rgba[i + 1] = 24;
            rgba[i + 2] = 24;
            rgba[i + 3] = 255;
        }
    }
    tray_icon::Icon::from_rgba(rgba, size, size).expect("build fallback tray icon")
}
