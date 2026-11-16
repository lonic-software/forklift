//! The window: a top bar that switches between the Run, Results and Coverage screens, and the
//! screens themselves, laid out as docs/proof-load-design-system.html composes them. The run is the
//! library's, on a worker thread (`run_state`); the report and a row's steps are the library's too
//! (`results`, `steps`). This file only lays them out.

use std::path::Path;

use eframe::egui::{self, Ui};
use proof_load::report::{Cell, Platform};
use proof_load::scenario::Tool;
use proof_load::run::RunError;

use crate::form::{Field, Form};
use crate::results::{self, Results};
use crate::run_state::{Ended, Event, Meter, Worker};
use crate::steps;
use crate::theme::{self, Palette, Type};
use crate::widgets::{self, Button, Column, ScreenLine};

/// The theme switch's options, in order.
const THEMES: [&str; 3] = ["System", "Light", "Dark"];

/// The screens, in the order the switch lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Run,
    Results,
    Coverage,
}

const SCREENS: [Screen; 3] = [Screen::Run, Screen::Results, Screen::Coverage];
const SCREEN_NAMES: [&str; 3] = ["Run", "Results", "Coverage"];

/// The most run messages the Run screen keeps on show, newest last.
const MESSAGES: usize = 6;

/// How many seconds a field's check stands before the file system is asked again.
const RECHECK: f64 = 1.0;

/// Whether a field check still stands: the form is as checked, and either `age` seconds is under
/// `RECHECK` or a run is going. A run fills its own report folder, so the check made as it
/// started stands until it ends, when the fields are checked again.
fn still_good(same_form: bool, age: f64, running: bool) -> bool {
    same_form && (running || age < RECHECK)
}

/// The Show switch's options: failing rows only, or every row.
const SHOW: [&str; 2] = ["Failures", "All"];

pub struct App {
    theme: usize,
    pub screen: Screen,
    pub form: Form,
    worker: Option<Worker>,
    pub meter: Meter,
    /// Why the last Open report was refused.
    opened: Option<String>,
    results: Option<Results>,
    /// 0 shows failures only, 1 every row.
    show: usize,
    /// 0 shows every binding, n the n-th of `Results::bindings`.
    binding: usize,
    /// The selected row, an index into `Results::rows`, and its step screen once read.
    selected: Option<usize>,
    step_lines: Option<(usize, Result<Vec<ScreenLine>, String>)>,
    /// The fields' errors, the form they were checked for and when, so the file system is asked
    /// again when a field changes, a run ends, or a second has passed, not every frame.
    checked: Option<(Form, f64, [Option<&'static str>; 6])>,
}

impl Default for App {
    fn default() -> App {
        App {
            theme: 0, screen: Screen::Run, form: Form::default(), worker: None, meter: Meter::default(),
            opened: None, results: None, show: 0, binding: 0, selected: None, step_lines: None, checked: None,
        }
    }
}

impl App {
    /// The window showing `results` on the Results screen, as after a run or Open report.
    pub fn showing(results: Results) -> App {
        let mut app = App::default();
        app.open(results);
        app
    }

    /// Selects the row of `scenario`, showing every row first if the filters hide it; false when
    /// no row has that name.
    pub fn select(&mut self, scenario: &str) -> bool {
        let Some(index) = self.results.as_ref().and_then(|results| results.rows.iter().position(|row| row.scenario == scenario)) else {
            return false;
        };
        if !self.shown().contains(&index) {
            (self.show, self.binding) = (1, 0);
        }
        self.selected = Some(index);
        true
    }

    fn open(&mut self, results: Results) {
        self.results = Some(results);
        self.show = 0;
        self.binding = 0;
        self.selected = None;
        self.step_lines = None;
        self.screen = Screen::Results;
        self.keep_selection();
    }

    /// The rows the table shows under the current filters.
    fn shown(&self) -> Vec<usize> {
        let Some(results) = &self.results else { return Vec::new() };
        let bindings = &results.bindings;
        let binding = self.binding.checked_sub(1).and_then(|index| bindings.get(index)).map(String::as_str);
        results.shown(self.show == 0, binding)
    }

    /// Keeps the selection on a shown row: the first one when the filters hid it.
    fn keep_selection(&mut self) {
        let shown = self.shown();
        if !self.selected.is_some_and(|selected| shown.contains(&selected)) {
            self.selected = shown.first().copied();
        }
    }

    /// One frame of the window, outside eframe, so tests can draw it into a bare `egui::Context`.
    pub fn show(&mut self, ctx: &egui::Context) {
        self.take_events();
        let p = theme::palette(ctx);
        egui::TopBottomPanel::top("top").frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin::symmetric(16, 12)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = theme::PLATE_GAP;
                    widgets::text(ui, "Proof Load", Type::Title, p.ink);
                    let mut screen = SCREENS.iter().position(|screen| *screen == self.screen).unwrap_or(0);
                    if widgets::segmented(ui, &mut screen, &SCREEN_NAMES).changed() {
                        self.screen = SCREENS[screen];
                    }
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
                    match self.screen {
                        Screen::Run => self.run_screen(ui),
                        Screen::Results => self.results_screen(ui),
                        Screen::Coverage => self.coverage_screen(ui),
                    }
                });
            });
    }

    /// Takes in what the worker sent since the last frame; a finished run opens its report.
    fn take_events(&mut self) {
        let Some(worker) = &self.worker else { return };
        let mut done = None;
        for event in worker.drain() {
            if let Event::Done(result) = &event {
                done = Some(result.clone());
            }
            self.meter.apply(event);
        }
        if let Some(result) = done {
            self.worker = None;
            // The run has written into its report folder, so the fields are checked again.
            self.checked = None;
            if let Ok(report) = result {
                self.open(Results::new(*report, Path::new(self.form.out.trim()).to_path_buf()));
            }
        }
    }

    /// The fields' errors, checked again when the form has changed since the last check, or
    /// `RECHECK` seconds after it, as files may come and go under the paths typed.
    fn errors(&mut self, now: f64) -> [Option<&'static str>; 6] {
        let running = self.running();
        match &self.checked {
            Some((form, at, errors)) if still_good(form == &self.form, now - at, running) => *errors,
            _ => {
                let errors = self.form.errors();
                self.checked = Some((self.form.clone(), now, errors));
                errors
            }
        }
    }

    fn running(&self) -> bool {
        self.worker.is_some()
    }

    fn run_screen(&mut self, ui: &mut Ui) {
        let p = theme::palette(ui.ctx());
        let running = self.running();
        let errors = self.errors(ui.input(|input| input.time));
        ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(RECHECK));
        widgets::plate(ui, "Run", "proof_load::run", |ui| {
            ui.add_enabled_ui(!running, |ui| {
                for (pair, pair_errors) in Field::ALL.chunks(2).zip(errors.chunks(2)) {
                    ui.columns(pair.len(), |columns| {
                        for ((column, field), error) in columns.iter_mut().zip(pair).zip(pair_errors) {
                            widgets::field(column, field.label(), self.form.value(*field), field.hint(), *error);
                        }
                    });
                }
            });
            ui.horizontal(|ui| {
                let ready = errors.iter().all(Option::is_none);
                if widgets::button(ui, "Run scenarios", Button::Primary, !running && ready).clicked() {
                    // Checked once more as it starts: the file system may have changed.
                    match self.form.options() {
                        Some(options) => {
                            let ctx = ui.ctx().clone();
                            self.meter = Meter::default();
                            self.opened = None;
                            self.worker = Some(Worker::start(options, move || ctx.request_repaint()));
                        }
                        // A field went wrong since its last check; the next frame shows which.
                        None => self.checked = None,
                    }
                }
                let stoppable = self.worker.as_ref().is_some_and(|worker| !worker.stopping());
                if widgets::button(ui, "Stop", Button::Default, stoppable).clicked() {
                    if let Some(worker) = &self.worker {
                        worker.stop();
                    }
                }
                let folder = self.form.out.trim().to_string();
                if widgets::button(ui, "Open report", Button::Default, !running && Path::new(&folder).is_absolute()).clicked() {
                    match Results::open(Path::new(&folder)) {
                        Ok(results) => {
                            self.opened = None;
                            self.open(results);
                        }
                        Err(error) => self.opened = Some(error),
                    }
                }
            });
            let (status, color) = self.status(p);
            if !status.is_empty() {
                widgets::text(ui, &status, Type::Body, color);
            }
        });
        if self.meter.lanes.is_empty() && self.meter.messages.is_empty() {
            return;
        }
        widgets::plate(ui, "Run meter", "one LED per scenario block", |ui| {
            for lane in &self.meter.lanes {
                widgets::led_meter(ui, lane.category.label(), &lane.leds());
            }
            let skip = self.meter.messages.len().saturating_sub(MESSAGES);
            for message in &self.meter.messages[skip..] {
                widgets::text(ui, message, Type::Small, p.muted);
            }
        });
    }

    /// The Run screen's status line, in its colour.
    fn status(&self, p: &Palette) -> (String, egui::Color32) {
        if let Some(worker) = &self.worker {
            let text = if worker.stopping() { "Stopping before the next block…" } else { "Running…" };
            return (text.to_string(), p.muted);
        }
        if let Some(error) = &self.opened {
            return (format!("Cannot open the report: {error}"), p.g_lost);
        }
        match &self.meter.ended {
            None => (String::new(), p.muted),
            Some(Ok(report)) => (format!("Finished: {} rows graded.", report.rows.len() + report.forklift_only.len()), p.muted),
            Some(Err(Ended::Run(RunError::Cancelled))) => ("Stopped. The run wrote no report.".to_string(), p.muted),
            Some(Err(Ended::Run(error))) => (format!("Refused: {error}"), p.g_lost),
            Some(Err(Ended::Internal(error))) => {
                let error = error.trim_end().trim_end_matches('.');
                (format!("Proof Load failed: {error}. This is a fault of the harness, not a grade."), p.g_lost)
            }
        }
    }

    fn results_screen(&mut self, ui: &mut Ui) {
        let p = theme::palette(ui.ctx());
        let Some(results) = &self.results else {
            widgets::plate(ui, "Results", "no report open", |ui| {
                widgets::text(ui, "Run scenarios, or open a report from the Run screen.", Type::Body, p.muted);
            });
            return;
        };
        let tally = results.tally();
        let bindings = results.bindings.clone();
        let out = results.out.display().to_string();
        let mut show = self.show;
        let mut binding = self.binding;
        let shown = self.shown();
        let mut clicked = None;
        widgets::plate(ui, "Results", &out, |ui| {
            ui.horizontal_top(|ui| {
                // `.screen`: the side column and the main one are 14 px apart.
                ui.spacing_mut().item_spacing.x = theme::PLATE_GAP;
                ui.allocate_ui_with_layout(egui::vec2(theme::SIDE_WIDTH, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(theme::SIDE_WIDTH);
                    ui.spacing_mut().item_spacing.y = theme::SIDE_GAP;
                    widgets::engraved(ui, "This run");
                    widgets::tally(ui, &tally);
                    widgets::segmented(ui, &mut show, &SHOW);
                    widgets::engraved(ui, "Binding");
                    let mut options = vec!["All"];
                    options.extend(bindings.iter().map(String::as_str));
                    // More bindings than the column holds scroll sideways rather than widen it.
                    egui::ScrollArea::horizontal().id_salt("bindings").show(ui, |ui| {
                        widgets::segmented(ui, &mut binding, &options);
                    });
                });
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = theme::STACK_GAP;
                    clicked = self.table(ui, &shown);
                    if shown.is_empty() {
                        widgets::text(ui, "No row under these filters.", Type::Body, p.muted);
                    }
                    if let Some(selected) = self.selected {
                        self.detail(ui, selected);
                    }
                });
            });
        });
        if let Some(row) = clicked {
            self.selected = Some(row);
        }
        if (show, binding) != (self.show, self.binding) {
            self.show = show;
            self.binding = binding;
            self.keep_selection();
        }
        if let Some(selected) = self.selected {
            self.step_screen(ui, selected);
        }
    }

    /// The results table; returns the row clicked, as an index into `Results::rows`.
    fn table(&self, ui: &mut Ui, shown: &[usize]) -> Option<usize> {
        let p = theme::palette(ui.ctx());
        let results = self.results.as_ref()?;
        let columns = [
            Column { heading: "Scenario", width: 0.0 },
            Column { heading: "Category", width: 170.0 },
            Column { heading: "Binding", width: 130.0 },
            Column { heading: "Grade", width: 180.0 },
            Column { heading: "To expected", width: 110.0 },
        ];
        let selected = self.selected.and_then(|selected| shown.iter().position(|index| *index == selected));
        let picked = widgets::table(ui, &columns, shown.len(), true, selected, |ui, row, column| {
            let row = &results.rows[shown[row]];
            match column {
                0 => { widgets::text(ui, &row.scenario, Type::Data, p.ink); }
                1 => { widgets::text(ui, row.category.label(), Type::Body, p.muted); }
                2 => { widgets::text(ui, &row.binding, Type::Data, p.ink); }
                3 => { widgets::grade_chip(ui, row.grade); }
                _ => {
                    let text = match row.recoverability.commands_to_expected {
                        None => "never".to_string(),
                        Some(count) => format!("{count} cmd"),
                    };
                    widgets::text(ui, &text, Type::Data, p.ink);
                }
            }
        });
        picked.map(|row| shown[row])
    }

    /// The selected row's readouts, as many to a line as fit at 220 px or more each.
    fn detail(&self, ui: &mut Ui, selected: usize) {
        let p = theme::palette(ui.ctx());
        let Some(row) = self.results.as_ref().map(|results| &results.rows[selected]) else { return };
        let (count, unit) = results::to_expected(row);
        let named = if row.recoverability.named_right_remedy { "yes" } else { "no" };
        let knowledge = results::outside_knowledge(row);
        let rescued = results::rescued_by(row);
        let width = ui.available_width();
        let readouts: [&dyn Fn(&mut Ui); 4] = [
            &|ui| { widgets::readout(ui, "Commands to expected", &count, unit); },
            &|ui| { widgets::readout(ui, "Named the remedy that worked", named, ""); },
            &|ui| { widgets::readout(ui, "Rescued by", rescued, ""); },
            &|ui| { widgets::readout_text(ui, "Outside knowledge", &knowledge); },
        ];
        let per_line = (((width + theme::SIDE_GAP) / (220.0 + theme::SIDE_GAP)).floor() as usize).clamp(1, readouts.len());
        let each = (width - theme::SIDE_GAP * (per_line as f32 - 1.0)) / per_line as f32;
        ui.spacing_mut().item_spacing.y = theme::SIDE_GAP;
        for line in readouts.chunks(per_line) {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = theme::SIDE_GAP;
                for readout in line {
                    ui.allocate_ui(egui::vec2(each, 0.0), |ui| {
                        ui.set_width(each);
                        readout(ui);
                    });
                }
            });
        }
        if let Some(note) = &row.note {
            widgets::text(ui, note, Type::Small, p.muted);
        }
    }

    /// The selected row's commands and what they printed, read from its details on first show.
    fn step_screen(&mut self, ui: &mut Ui, selected: usize) {
        let p = theme::palette(ui.ctx());
        let Some(results) = &self.results else { return };
        let row = &results.rows[selected];
        if self.step_lines.as_ref().is_none_or(|(index, _)| *index != selected) {
            let lines = row.steps(&results.out).map(|outcomes| steps::lines(&outcomes));
            self.step_lines = Some((selected, lines));
        }
        let tag = row.scenario.clone();
        let Some((_, lines)) = &self.step_lines else { return };
        widgets::plate(ui, "Steps", &tag, |ui| match lines {
            Ok(lines) => { widgets::screen(ui, lines); }
            Err(error) => { widgets::text(ui, &format!("The steps cannot be shown: {error}"), Type::Body, p.g_lost); }
        });
    }

    fn coverage_screen(&mut self, ui: &mut Ui) {
        let p = theme::palette(ui.ctx());
        let Some(results) = &self.results else {
            widgets::plate(ui, "Coverage", "no report open", |ui| {
                widgets::text(ui, "Run scenarios, or open a report from the Run screen.", Type::Body, p.muted);
            });
            return;
        };
        let coverage = &results.report.coverage;
        widgets::plate(ui, "Coverage", "report::CoverageTable", |ui| {
            let mut columns = vec![Column { heading: "Category", width: 0.0 }];
            columns.extend(Platform::ALL.map(|platform| Column { heading: platform.label(), width: 200.0 }));
            let categories: Vec<_> = coverage.cells.keys().copied().collect();
            widgets::table(ui, &columns, categories.len(), false, None, |ui, row, column| {
                let category = categories[row];
                if column == 0 {
                    let mut label = category.label().to_string();
                    if let Some(count) = coverage.not_reached.get(&category) {
                        label.push_str(&format!(" ({count} not reached)"));
                    }
                    widgets::text(ui, &label, Type::Body, p.ink);
                } else {
                    match coverage.cells[&category].get(&Platform::ALL[column - 1]) {
                        Some(cell) => { widgets::coverage_cell(ui, cell); }
                        None => { widgets::text(ui, "not in the report", Type::Cell, p.muted); }
                    }
                }
            });
        });
        let completeness = &results.report.completeness;
        widgets::plate(ui, "Completeness", "what the run checked it covered", |ui| {
            let columns = [
                Column { heading: "Check", width: 260.0 },
                Column { heading: "State", width: 200.0 },
                Column { heading: "Missing", width: 0.0 },
            ];
            widgets::table(ui, &columns, completeness.len(), false, None, |ui, row, column| {
                let check = &completeness[row];
                match column {
                    0 => {
                        let label = match check.subject { Some(subject) => format!("{} ({})", check.check, subject.label()), None => check.check.clone() };
                        widgets::text(ui, &label, Type::Body, p.ink);
                    }
                    1 => { widgets::coverage_cell(ui, &check.cell); }
                    _ if check.missing.is_empty() => {
                        let nothing = if matches!(check.cell, Cell::Run) { "nothing" } else { "" };
                        widgets::text(ui, nothing, Type::Body, p.muted);
                    }
                    // A git subject's gap is a comparison not fully measured, never a failure.
                    _ if check.subject.is_some_and(|subject| subject != Tool::Forklift) => {
                        widgets::text(ui, &format!("not fully measured: {}", check.missing.join("; ")), Type::Body, p.muted);
                    }
                    _ => { widgets::text(ui, &check.missing.join("; "), Type::Body, p.g_lost); }
                }
            });
        });
        let determinism = &results.report.determinism;
        widgets::plate(ui, "Replay", "what determines this run", |ui| {
            widgets::text(ui, &format!("seed {} on {} ({}), filesystem {}", determinism.seed, determinism.platform.label(), determinism.os, determinism.filesystem), Type::Data, p.ink);
            for (tool, version) in &determinism.tools {
                widgets::text(ui, &format!("{tool}: {version}"), Type::Data, p.muted);
            }
            for (subject, configuration) in &determinism.configurations {
                widgets::text(ui, &format!("{subject} configuration: {configuration}"), Type::Data, p.muted);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A check stands while the form is unchanged and young, or while a run is going whatever its
    /// age, and is made again otherwise.
    ///
    /// Revert: re-check by age while running → a run's own files turn its folder field red.
    #[test]
    fn a_check_stands_through_a_run() {
        assert!(still_good(true, 0.5, false));
        assert!(!still_good(true, RECHECK, false));
        assert!(still_good(true, 60.0, true), "a run's own report folder is not checked under it");
        assert!(!still_good(false, 0.0, true));
    }
}
