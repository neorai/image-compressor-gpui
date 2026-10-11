#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod domain;
mod infrastructure;
mod ports;
mod ui;

use std::error::Error;
use std::process::ExitCode;
use std::sync::Arc;

use app::AppController;
use infrastructure::paths::AppPaths;

fn main() -> ExitCode {
    let controller = match start() {
        Ok(controller) => Arc::new(controller),
        Err(error) => {
            report_startup_error(&*error);
            return ExitCode::FAILURE;
        }
    };

    ui::run(Arc::clone(&controller));

    if let Err(error) = controller.shutdown() {
        eprintln!("failed to stop the background job cleanly: {error}");
    }
    ExitCode::SUCCESS
}

fn start() -> Result<AppController, Box<dyn Error>> {
    let paths = AppPaths::discover()?;
    Ok(AppController::new(paths)?)
}

/// Release builds have no console, so a startup failure such as a read-only
/// executable folder is shown in a native dialog instead of being lost.
fn report_startup_error(error: &dyn Error) {
    eprintln!("Image Compressor could not start: {error}");
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("Image Compressor")
        .set_description(format!(
            "The application could not start.\n\n{error}\n\nThe folder that contains the executable must be writable, because local data is stored in its \"data\" subfolder."
        ))
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}
