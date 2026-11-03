#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use gpui::{App, AppContext, Application, Bounds, WindowBounds, WindowOptions, px, size};

use spritx_terminal::assets::Assets;
use spritx_terminal::memprof;
use spritx_terminal::screens::app_shell::SpritXApp;

fn main() {
    // Must be the first call so the reporter thread and the counting global
    // allocator are live before the app builds any of its heavy buffers.
    memprof::init();

    Application::new()
        .with_assets(Assets)
        .run(|cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(1100.0), px(700.0)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| {
                    let app = cx.new(|cx| SpritXApp::new(cx));
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
            )
            .expect("open SpritX window");
            cx.activate(true);

            if std::env::var_os("SPRITX_SMOKE").is_some() {
                cx.spawn(async move |cx| {
                    gpui::Timer::after(std::time::Duration::from_secs(3)).await;
                    let _ = cx.update(|cx| cx.quit());
                })
                .detach();
            }
        });

    memprof::report_now();
}
