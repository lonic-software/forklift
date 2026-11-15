//! Opens the Proof Load window. Everything it shows is built in the library.

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1120.0, 760.0]).with_min_inner_size([760.0, 480.0]),
        ..eframe::NativeOptions::default()
    };
    eframe::run_native(
        "Proof Load",
        options,
        Box::new(|creation| {
            proof_load_desktop::install(&creation.egui_ctx);
            Ok(Box::new(proof_load_desktop::app::App::default()))
        }),
    )
}
