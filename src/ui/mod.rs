mod format;
mod i18n;
mod messages;
mod view;

use std::sync::Arc;

use gpui_kit::component::Theme;
use gpui_kit::{AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

use crate::app::AppController;
use view::CompressorView;

/// Runs the GPUI event loop until the last window closes.
pub fn run(controller: Arc<AppController>) {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            Theme::sync_system_appearance(None, cx);

            // Quitting cancels an active run and waits for the current image,
            // so no temporary file is left behind by an abrupt exit.
            let quit_controller = Arc::clone(&controller);
            cx.on_app_quit(move |_| {
                if let Err(error) = quit_controller.shutdown() {
                    eprintln!("failed to stop the background job cleanly: {error}");
                }
                async {}
            })
            .detach();
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Image Compressor".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1120.), px(760.)),
                    cx,
                ))),
                window_min_size: Some(size(px(860.), px(640.))),
                ..Default::default()
            };
            let opened = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| CompressorView::new(controller, window, cx))
            });
            if let Err(error) = opened {
                eprintln!("the main window could not be opened: {error}");
                cx.quit();
                return;
            }
            cx.activate(true);
        });
}
