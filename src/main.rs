use gpui::{App, AppContext, Application, Bounds, WindowBounds, WindowOptions, px, size};

use spritx_terminal::assets::Assets;
use spritx_terminal::screens::app_shell::SpritXApp;

fn main() {
    Application::new()
        .with_assets(Assets)
        .run(|cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(1100.0), px(700.0)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| cx.new(|cx| SpritXApp::new(cx)),
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
}
