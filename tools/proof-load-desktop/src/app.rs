//! The window. For now it shows the design system's components on a bench, as the page lays them
//! out, so the egui rendering can be held against the page; the Run, Results, Coverage and step
//! screens replace the bench next.

use eframe::egui::{self, Ui};
use proof_load::oracle::Grade;
use proof_load::report::Cell;

use crate::grades;
use crate::theme::{self, Type};
use crate::widgets::{self, Button, Ink, Led, ScreenLine};

/// The theme switch's options, in order.
const THEMES: [&str; 3] = ["System", "Light", "Dark"];

pub struct App {
    theme: usize,
    binding: usize,
    show: usize,
    only: String,
    forklift: String,
}

impl Default for App {
    fn default() -> App {
        App { theme: 0, binding: 0, show: 0, only: "redacted".to_string(), forklift: "target/debug/forklfit".to_string() }
    }
}

impl App {
    /// One frame of the window, outside eframe, so tests can draw it into a bare `egui::Context`.
    pub fn show(&mut self, ctx: &egui::Context) {
        let p = theme::palette(ctx);
        egui::TopBottomPanel::top("top").frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin::symmetric(16, 12)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    widgets::text(ui, "Proof Load", Type::Title, p.ink);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if widgets::segmented(ui, &mut self.theme, &THEMES).changed() {
                            ctx.set_theme(theme_preference(self.theme));
                        }
                        widgets::engraved(ui, "Theme");
                    });
                });
            });
        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin::symmetric(16, 8)))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = theme::SCREEN_GAP;
                    self.bench(ui);
                });
            });
    }

    fn bench(&mut self, ui: &mut Ui) {
        let p = theme::palette(ui.ctx());
        widgets::plate(ui, "Grades", "worst first", |ui| {
            ui.horizontal_wrapped(|ui| {
                for grade in grades::WORST_FIRST {
                    widgets::grade_chip(ui, grade);
                }
            });
        });
        widgets::plate(ui, "Controls", "every state", |ui| {
            ui.horizontal_wrapped(|ui| {
                widgets::button(ui, "Run scenarios", Button::Primary, true);
                widgets::button(ui, "Run scenarios", Button::Primary, false);
                widgets::button(ui, "Open report", Button::Default, true);
                widgets::button(ui, "Open report", Button::Default, false);
                widgets::segmented(ui, &mut self.binding, &["forklift", "git", "git-tuned"]);
            });
            ui.horizontal(|ui| {
                ui.allocate_ui(egui::vec2(260.0, 64.0), |ui| {
                    widgets::field(ui, "Only", &mut self.only, "Scenarios whose name contains this text", None)
                });
                ui.allocate_ui(egui::vec2(260.0, 64.0), |ui| {
                    widgets::field(ui, "forklift binary", &mut self.forklift, "", Some("No file at this path."))
                });
            });
        });
        widgets::plate(ui, "Run meter", "one LED per scenario block", |ui| {
            let leds: Vec<Led> = (0..40)
                .map(|index| match index {
                    9 | 17 => Led::Failed,
                    0..=25 => Led::Passed,
                    26 => Led::Running,
                    _ => Led::Queued,
                })
                .collect();
            widgets::led_meter(ui, "Damaged files", &leds);
        });
        widgets::plate(ui, "Step screen", "what the tool printed", |ui| {
            widgets::screen(ui, &[
                ScreenLine::command("forklift git export --out ./x"),
                ScreenLine::new(Ink::Red, "error: the applied-redactions record is damaged."),
                ScreenLine::new(Ink::Dim, "  next step:").then(Ink::Plain, " forklift heal"),
                ScreenLine::command("forklift heal"),
                ScreenLine::new(Ink::Plain, "rebuilt the applied-redactions record (2 kept)"),
                ScreenLine::new(Ink::Amber, "applied again: 1 redaction"),
                ScreenLine::command("forklift git export --out ./x").then(Ink::Plain, "   exit 0"),
            ]);
        });
        widgets::plate(ui, "Coverage cells", "report::CoverageTable", |ui| {
            ui.horizontal(|ui| {
                widgets::coverage_cell(ui, &Cell::Run);
                widgets::coverage_cell(ui, &Cell::Sampled);
                widgets::coverage_cell(ui, &Cell::not_run("no syscall injection on Windows in this harness"));
            });
        });
        widgets::plate(ui, "Results parts", "tally, filter and readouts", |ui| {
            ui.horizontal_top(|ui| {
                ui.allocate_ui(egui::vec2(230.0, 200.0), |ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = theme::SIDE_GAP;
                        widgets::engraved(ui, "This run");
                        let counts: Vec<_> = grades::WORST_FIRST.into_iter().zip([1, 4, 3, 61, 343]).collect();
                        widgets::tally(ui, &counts);
                        widgets::segmented(ui, &mut self.show, &["Failures", "All"]);
                    });
                });
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = theme::SIDE_GAP;
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = theme::SIDE_GAP;
                        ui.allocate_ui(egui::vec2(220.0, 80.0), |ui| widgets::readout(ui, "Commands to expected", "3", "commands"));
                        ui.allocate_ui(egui::vec2(220.0, 80.0), |ui| widgets::readout(ui, "Named the remedy that worked", "yes", ""));
                    });
                    widgets::text(ui, "redactions-applied__flip__byte-17", Type::Data, p.ink);
                    widgets::grade_chip(ui, Grade::RefusedWorkingRemedy);
                });
            });
        });
    }
}

/// The theme switch's option as egui's preference.
fn theme_preference(index: usize) -> egui::ThemePreference {
    match index {
        1 => egui::ThemePreference::Light,
        2 => egui::ThemePreference::Dark,
        _ => egui::ThemePreference::System,
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.show(ctx);
    }
}
