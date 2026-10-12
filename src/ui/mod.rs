mod eta;
mod format;
mod i18n;
mod messages;
mod view;

use std::sync::Arc;

use gpui_kit::component::{Theme, ThemeRegistry};
use gpui_kit::{
    App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};

use crate::app::AppController;
use view::CompressorView;

/// The interface palette, after <https://draculatheme.com>.
const DRACULA_THEME: &str = include_str!("dracula.json");
const DRACULA_THEME_NAME: &str = "Dracula";

/// Runs the GPUI event loop until the last window closes.
pub fn run(controller: Arc<AppController>) {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            apply_dracula_theme(cx);

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

/// Installs the Dracula palette. Dracula is dark only, so it does not follow
/// the system appearance; if the bundled theme cannot be loaded, the default
/// GPUI Kit theme for the system appearance is kept.
fn apply_dracula_theme(cx: &mut App) {
    if let Err(error) = ThemeRegistry::global_mut(cx).load_themes_from_str(DRACULA_THEME) {
        eprintln!("the Dracula theme could not be loaded: {error}");
        Theme::sync_system_appearance(None, cx);
        return;
    }
    let Some(config) = ThemeRegistry::global(cx)
        .themes()
        .get(DRACULA_THEME_NAME)
        .cloned()
    else {
        eprintln!("the Dracula theme is missing from its theme file");
        Theme::sync_system_appearance(None, cx);
        return;
    };
    Theme::update(cx, |theme| theme.apply_config(&config));
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::ThemeSet;

    use super::{DRACULA_THEME, DRACULA_THEME_NAME};

    #[test]
    fn dracula_theme_file_is_a_valid_dark_theme() -> Result<(), serde_json::Error> {
        let set: ThemeSet = serde_json::from_str(DRACULA_THEME)?;
        assert_eq!(set.themes.len(), 1);
        assert_eq!(set.themes[0].name.as_ref(), DRACULA_THEME_NAME);
        assert!(set.themes[0].mode.is_dark());
        Ok(())
    }
}
