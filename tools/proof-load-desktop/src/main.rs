//! Opens the Proof Load window. Everything it shows is built in the library.
//!
//! `proof-load-desktop [<report folder>]`: given the folder a run wrote its report to, the window
//! opens on that report's results; a folder whose report cannot be read is refused before any
//! window opens.

use std::process::ExitCode;

use proof_load_desktop::app::App;
use proof_load_desktop::results::Results;

fn main() -> ExitCode {
    let app = match std::env::args_os().nth(1) {
        None => App::default(),
        Some(folder) => match Results::open(std::path::Path::new(&folder)) {
            Ok(results) => App::showing(results),
            Err(error) => {
                eprintln!("proof-load-desktop: {error}");
                return ExitCode::from(2);
            }
        },
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1120.0, 760.0]).with_min_inner_size([760.0, 480.0]),
        ..eframe::NativeOptions::default()
    };
    let opened = eframe::run_native(
        "Proof Load",
        options,
        Box::new(|creation| {
            proof_load_desktop::install(&creation.egui_ctx);
            Ok(Box::new(app))
        }),
    );
    match opened {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("proof-load-desktop: {error}");
            ExitCode::FAILURE
        }
    }
}
