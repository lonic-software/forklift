//! Proof Load's desktop head: an egui window over the proof-load library. It is a view only. The
//! run, the grades and the report are the library's (`proof_load::run`, `proof_load::report`),
//! and every colour, size and face comes from `theme`, which follows
//! docs/proof-load-design-system.html.

pub mod app;
pub mod fonts;
pub mod form;
pub mod grades;
pub mod results;
pub mod run_state;
pub mod steps;
pub mod theme;
pub mod widgets;

use eframe::egui;

/// Gives `ctx` the embedded fonts and the page's look in both themes. egui follows the system theme
/// until `ctx.set_theme` picks one.
pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(fonts::definitions());
    for theme in [egui::Theme::Light, egui::Theme::Dark] {
        ctx.set_visuals_of(theme, theme::visuals(theme));
        ctx.style_mut_of(theme, theme::spacing);
    }
}
