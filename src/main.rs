#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use gpui::{px, size, App, AppContext, Application, Bounds, WindowBounds, WindowOptions};

use spritx_terminal::assets::Assets;
use spritx_terminal::desktop::{self, Claim, DesktopAction, Instance, Tray};
use spritx_terminal::memprof::{self, TrackingAlloc};
use spritx_terminal::screens::app_shell::SpritXApp;

/// The counting allocator lives in `memprof`, but it is registered here rather
/// than in the library: a `#[global_allocator]` in the lib would be inherited by
/// every binary and test target linked against it, and any future allocator in a
/// binary target would then be a duplicate lang item. `memprof`'s bookkeeping is
/// inert until `memprof::init()` turns it on.
#[global_allocator]
static ALLOC: TrackingAlloc = TrackingAlloc;

struct DesktopResources {
    instance: Instance,
    tray: Option<Tray>,
}
/// Surface a fatal startup error on release Windows, where the
/// `windows_subsystem = "windows"` build has no console: stderr plus a line in
/// the log under the user config directory.
fn report_fatal(message: &str) {
    eprintln!("[spritx] {message}");
    if let Some(dir) = spritx_terminal::platform::config_dir() {
        use std::io::Write;
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("spritx.log"))
            .and_then(|mut file| writeln!(file, "[spritx] {message}"));
    }
}

/// Installed first, so a panic anywhere (not just the startup `expect`s) still
/// leaves a visible record instead of a silent exit code on release Windows.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let message = if let Some(text) = info.payload().downcast_ref::<&str>() {
            (*text).to_string()
        } else if let Some(text) = info.payload().downcast_ref::<String>() {
            text.clone()
        } else {
            "unknown panic".to_string()
        };
        let location = info
            .location()
            .map(|loc| format!("{}:{}", loc.file(), loc.line()))
            .unwrap_or_else(|| "unknown location".to_string());
        report_fatal(&format!("panic at {location}: {message}"));
    }));
}

fn main() {
    // Must be the first call so the reporter thread and the counting global
    // allocator are live before the app builds any of its heavy buffers.
    memprof::init();
    install_panic_hook();

    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == "--session-owner") {
        let result = args
            .get(2)
            .ok_or_else(|| anyhow::anyhow!("Session path missing"))
            .and_then(|path| {
                spritx_terminal::backend::session_owner::run(std::path::Path::new(path))
            });
        if result.is_err() {
            std::process::exit(1);
        }
        return;
    }
    // No `expect` here: on release Windows this build has no console, so a
    // panic would exit silently with code 101. Log + exit instead.
    let Some(config_root) = spritx_terminal::platform::config_dir() else {
        report_fatal("could not determine the user config directory");
        std::process::exit(1);
    };
    let desktop_root = config_root.join("sessions");
    let instance = match Instance::claim(&desktop_root) {
        Ok(Claim::Primary(instance)) => instance,
        Ok(Claim::Reopened) => return,
        Err(error) => {
            report_fatal(&format!("could not start SpritX desktop instance: {error:#}"));
            std::process::exit(1);
        }
    };

    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            let tray = Tray::new().map_err(|error| eprintln!("[spritx] tray unavailable: {error}")).ok();
            let tray_available = tray.is_some();
            let bounds = Bounds::centered(None, size(px(1100.0), px(700.0)), cx);
            let main_window = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    app_id: Some(std::env::var("FLATPAK_ID").unwrap_or_else(|_| "spritx-terminal".into())),
                    ..Default::default()
                },
                |window, cx| {
                    let app = cx.new(|cx| SpritXApp::new(cx));
                    let weak = app.downgrade();
                    window.on_window_should_close(cx, move |window, cx| {
                        let Ok(close_to_tray) = weak.update(cx, |app, _| {
                            if app.settings.close_to_tray { app.prepare_for_background(); }
                            app.settings.close_to_tray
                        }) else { return true; };
                        if !close_to_tray { return true; }
                        if tray_available { desktop::background_window(window); }
                        else { window.minimize_window(); }
                        false
                    });
                    let appearance = spritx_terminal::terminal::appearance();
                    eprintln!(
                        "[spritx] executable={} config={} terminal-font={}px family={} display-scale={}x",
                        std::env::current_exe()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|_| "unknown".into()),
                        spritx_terminal::backend::config::config_path()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "unavailable".into()),
                        appearance.font_size,
                        appearance.font_family,
                        window.scale_factor(),
                    );
                    app
                },
            );
            let main_window = match main_window {
                Ok(window) => window,
                Err(error) => {
                    report_fatal(&format!("could not open SpritX window: {error:#}"));
                    std::process::exit(1);
                }
            };
            cx.activate(true);

            #[cfg(windows)]
            if std::env::var_os("SPRITX_DESKTOP_TEST").is_some() {
                desktop::lifecycle_test(
                    main_window, tray_available,
                    tray.as_ref().map(Tray::native_window).unwrap_or(0), cx,
                );
            }

            let resources = std::rc::Rc::new(std::cell::RefCell::new(Some(DesktopResources { instance, tray })));
            let cleanup = resources.clone();
            cx.on_app_quit(move |_| {
                cleanup.borrow_mut().take();
                async {}
            }).detach();

            cx.spawn(async move |cx| {
                loop {
                    gpui::Timer::after(std::time::Duration::from_millis(100)).await;
                    let actions = {
                        let resources = resources.borrow();
                        let Some(resources) = resources.as_ref() else { return; };
                        let mut actions: Vec<_> = resources.instance.events.try_iter().collect();
                        if let Some(tray) = &resources.tray { actions.extend(tray.poll()); }
                        actions
                    };
                    let mut quitting = false;
                    for action in actions {
                        if main_window.update(cx, |app, window, cx| match action {
                            DesktopAction::Show => desktop::show_window(window),
                            DesktopAction::Quit => { quitting = true; app.quit_desktop(cx); }
                        }).is_err() { return; }
                    }
                    if quitting { return; }
                }
            }).detach();

            #[cfg(debug_assertions)]
            if std::env::var_os("SPRITX_TAB_TEST").is_some() {
                spritx_terminal::screens::app_shell::tab_lifecycle_test(main_window, cx);
            }

            #[cfg(debug_assertions)]
            if std::env::var_os("SPRITX_PROCESS_TEST").is_some() {
                spritx_terminal::screens::app_shell::process_control_test(main_window, cx);
            }

            if std::env::var_os("SPRITX_SMOKE").is_some() && std::env::var_os("SPRITX_TAB_TEST").is_none() {
                cx.spawn(async move |cx| {
                    gpui::Timer::after(std::time::Duration::from_secs(3)).await;
                    let _ = cx.update(|cx| cx.quit());
                })
                .detach();
            }
        });

    memprof::report_now();
}
