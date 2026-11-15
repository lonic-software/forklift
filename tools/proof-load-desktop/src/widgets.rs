//! The page's components, drawn with egui: plates, engraved labels, buttons, the segmented control,
//! fields, lamps and coverage cells, grade glyphs and chips, the LED run meter, readouts and the
//! step screen. Each takes its colours and type from `theme` and holds no state of its own beyond
//! what egui keeps for focus and hover. Raised and sunk pads are a 1 px highlight and a 1 px shade
//! stroke, as the page's note says, not real inset shadows.

use eframe::egui::{self, text::LayoutJob, Color32, CornerRadius, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, Vec2};
use proof_load::oracle::Grade;
use proof_load::report::Cell;

use crate::grades;
use crate::theme::{self, hairline, Palette, Type, U};

/// Text in one of the page's type styles. A `Label` is drawn upper case, as `.engraved` is.
pub fn job(text: &str, style: Type, color: Color32) -> LayoutJob {
    let text = if style == Type::Label { text.to_uppercase() } else { text.to_string() };
    LayoutJob::single_section(text, style.format(color))
}

/// A line of text that does not respond to the pointer.
pub fn text(ui: &mut Ui, text: &str, style: Type, color: Color32) -> Response {
    ui.add(egui::Label::new(job(text, style, color)).selectable(false))
}

/// An engraved label: 10.5/600, upper case, letter-spaced, muted.
pub fn engraved(ui: &mut Ui, label: &str) -> Response {
    let muted = theme::palette(ui.ctx()).muted;
    text(ui, label, Type::Label, muted)
}

/// A raised pad: its fill, a highlight along the top and, in light, a shade along the bottom.
pub fn raised(ctx: &egui::Context, rect: Rect, fill: Color32, radius: u8) -> Vec<Shape> {
    let mut shapes = vec![Shape::rect_filled(rect, CornerRadius::same(radius), fill)];
    shapes.extend(raised_edges(ctx, rect, radius));
    shapes
}

/// A raised pad's edges without its fill, for a frame that paints its own.
fn raised_edges(ctx: &egui::Context, rect: Rect, radius: u8) -> Vec<Shape> {
    let p = theme::palette(ctx);
    let mut shapes = vec![edge(rect, radius, true, p.pad_hi)];
    shapes.extend(theme::raise_shade(ctx.theme()).map(|shade| edge(rect, radius, false, shade)));
    shapes
}

/// A sunk well: its fill, a shade along the top, and in light a highlight along the bottom.
pub fn sunk(ctx: &egui::Context, rect: Rect, fill: Color32, radius: u8) -> Vec<Shape> {
    let p = theme::palette(ctx);
    let mut shapes = vec![Shape::rect_filled(rect, CornerRadius::same(radius), fill), edge(rect, radius, true, p.pad_lo)];
    shapes.extend(theme::sunk_lip(ctx.theme()).map(|lip| edge(rect, radius, false, lip)));
    shapes
}

/// A 1 px line along the straight part of a pad's top or bottom edge, inside it.
fn edge(rect: Rect, radius: u8, top: bool, color: Color32) -> Shape {
    let inset = f32::from(radius);
    let y = if top { rect.top() + 0.5 } else { rect.bottom() - 0.5 };
    Shape::hline(rect.left() + inset..=rect.right() - inset, y, hairline(color))
}

/// Draws a sunk well behind contents laid out by `add_contents`, sized to them.
fn in_well<R>(ui: &mut Ui, margin: egui::Margin, add_contents: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    let p = theme::palette(ui.ctx());
    let slot = ui.painter().add(Shape::Noop);
    let inner = egui::Frame::new().inner_margin(margin).show(ui, add_contents);
    ui.painter().set(slot, Shape::Vec(sunk(ui.ctx(), inner.response.rect, p.well, theme::R_S)));
    inner
}

/// The focus ring: 2 px of `--focus`, `offset` outside the rect.
fn focus_ring(ui: &Ui, rect: Rect, radius: u8, offset: f32) {
    let focus = theme::palette(ui.ctx()).focus;
    let ring = rect.expand(offset + 1.0);
    ui.painter().rect_stroke(ring, CornerRadius::same(radius.saturating_add(offset as u8)), Stroke::new(2.0_f32, focus), StrokeKind::Middle);
}

/// A plate: a raised panel with a heading, an engraved tag on the right and a rule under them.
pub fn plate<R>(ui: &mut Ui, heading: &str, tag: &str, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    let p = theme::palette(ui.ctx());
    let frame = egui::Frame::new().fill(p.panel).corner_radius(CornerRadius::same(theme::R_L))
        .inner_margin(egui::Margin::same(theme::PLATE_PADDING as i8));
    let inner = frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        // The plate sets its own gaps, so none of the screen's spacing reaches its contents.
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.horizontal(|ui| {
            text(ui, heading, Type::Heading, p.ink);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| engraved(ui, tag));
        });
        ui.add_space(theme::HEADER_GAP);
        let rule = ui.available_rect_before_wrap();
        ui.painter().hline(rule.x_range(), rule.top(), hairline(p.line));
        ui.add_space(theme::PLATE_GAP);
        // The plate's parts are PLATE_GAP apart; a layout inside a part sets its own page gap.
        ui.spacing_mut().item_spacing.y = theme::PLATE_GAP;
        add_contents(ui)
    });
    ui.painter().extend(raised_edges(ui.ctx(), inner.response.rect, theme::R_L));
    inner.inner
}

/// How a button is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    /// The amber action: Run scenarios.
    Primary,
    /// A raised pad: Open report, Stop.
    Default,
}

/// A button in one of the page's two kinds. A disabled one draws at 45% and does not respond.
pub fn button(ui: &mut Ui, label: &str, kind: Button, enabled: bool) -> Response {
    let p = theme::palette(ui.ctx());
    let (fill, ink, style) = match kind {
        Button::Primary => (p.accent, p.accent_ink, Type::PrimaryControl),
        Button::Default => (p.pad, p.ink, Type::Control),
    };
    // The text's colour is the job's own, which the painter's fallback colour does not replace.
    let ink = if enabled { ink } else { theme::disabled(ink) };
    let galley = ui.painter().layout_job(job(label, style, ink));
    let padding = egui::vec2(3.5 * U, 0.0);
    let size = egui::vec2(galley.size().x + 2.0 * padding.x, theme::CONTROL_HEIGHT);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let pressed = enabled && response.is_pointer_button_down_on();
        let fill = match (kind, pressed, enabled && response.hovered()) {
            (Button::Default, true, _) => p.well,
            (Button::Default, false, true) => p.pad_hi,
            (Button::Primary, false, true) => theme::hovered_primary(fill),
            _ => fill,
        };
        let shift = if pressed { egui::vec2(0.0, 1.0) } else { Vec2::ZERO };
        if !enabled {
            ui.painter().rect_filled(rect, CornerRadius::same(theme::R_S), theme::disabled(fill));
        } else if pressed {
            ui.painter().extend(sunk(ui.ctx(), rect.translate(shift), fill, theme::R_S));
        } else {
            ui.painter().extend(raised(ui.ctx(), rect, fill, theme::R_S));
        }
        let text_pos = rect.center() - galley.size() / 2.0 + shift;
        ui.painter().galley(text_pos, galley, ink);
        if response.has_focus() {
            focus_ring(ui, rect, theme::R_S, 2.0);
        }
    }
    response
}

/// The segmented control: one option pressed at a time, in a sunk well. Returns the response of
/// the whole control, changed when `selected` moved.
pub fn segmented(ui: &mut Ui, selected: &mut usize, options: &[&str]) -> Response {
    let p = theme::palette(ui.ctx());
    let inset = 3.0;
    let galleys: Vec<_> = options.iter().map(|option| ui.painter().layout_job(job(option, Type::Chip, p.ink))).collect();
    let widths: Vec<f32> = galleys.iter().map(|galley| galley.size().x + 2.0 * 11.0).collect();
    let height = theme::CONTROL_HEIGHT - 2.0;
    let size = egui::vec2(widths.iter().sum::<f32>() + inset * (options.len() as f32 + 1.0), height);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().extend(sunk(ui.ctx(), rect, p.well, theme::R_S));
    let mut x = rect.left() + inset;
    for (index, (galley, width)) in galleys.into_iter().zip(widths).enumerate() {
        let segment = Rect::from_min_size(Pos2::new(x, rect.top() + inset), egui::vec2(width, height - 2.0 * inset));
        let id = response.id.with(index);
        let option = ui.interact(segment, id, Sense::click());
        if option.clicked() && *selected != index {
            *selected = index;
            response.mark_changed();
        }
        let pressed = *selected == index;
        if pressed {
            ui.painter().extend(raised(ui.ctx(), segment, p.pad, theme::SEGMENT_RADIUS));
        }
        // Laid out before the click is read, so the colour is set here; a galley's own colour
        // would win over the painter's fallback.
        let ink = if pressed { p.ink } else { p.muted };
        ui.painter().galley_with_override_text_color(segment.center() - galley.size() / 2.0, galley, ink);
        if option.has_focus() {
            focus_ring(ui, segment, theme::SEGMENT_RADIUS, 2.0);
        }
        x += width + inset;
    }
    response
}

/// A labelled path or filter field. With an error the well gets a `--g-lost` edge and the error
/// replaces the hint, in `--g-lost`.
pub fn field(ui: &mut Ui, label: &str, value: &mut String, hint: &str, error: Option<&str>) -> Response {
    let p = theme::palette(ui.ctx());
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 5.0;
        engraved(ui, label);
        let width = ui.available_width();
        let edit = egui::TextEdit::singleline(value).font(Type::Input.font()).text_color(p.ink).frame(false)
            .margin(egui::Margin::symmetric(10, 0)).vertical_align(egui::Align::Center)
            .min_size(egui::vec2(width, theme::CONTROL_HEIGHT)).desired_width(width);
        let slot = ui.painter().add(Shape::Noop);
        let response = ui.add(edit);
        let well = response.rect;
        let mut shapes = sunk(ui.ctx(), well, p.well, theme::R_S);
        if error.is_some() {
            shapes.push(Shape::rect_stroke(well, CornerRadius::same(theme::R_S), Stroke::new(1.5_f32, p.g_lost), StrokeKind::Inside));
        }
        ui.painter().set(slot, Shape::Vec(shapes));
        if response.has_focus() {
            focus_ring(ui, well, theme::R_S, 1.0);
        }
        match error {
            Some(error) => text(ui, error, Type::Small, p.g_lost),
            None => text(ui, hint, Type::Small, p.muted),
        };
        response
    })
    .inner
}

/// A lamp's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lamp {
    Off,
    /// A coverage cell that ran: filled `--g-healed`.
    Run,
    /// A sampled cell: a hollow `--sampled` ring.
    Sampled,
    /// A cell that did not run: filled `--notrun`.
    NotRun,
}

/// A 12 px lamp at `center`, with the page's faint inset ring; a sampled lamp is its ring alone.
pub fn paint_lamp(ui: &Ui, center: Pos2, lamp: Lamp) {
    let p = theme::palette(ui.ctx());
    let painter = ui.painter();
    let radius = 6.0;
    let fill = match lamp {
        Lamp::Off => p.lamp_off,
        Lamp::Run => p.g_healed,
        Lamp::NotRun => p.notrun,
        Lamp::Sampled => {
            painter.circle_stroke(center, radius - 1.0, Stroke::new(2.0_f32, p.sampled));
            return;
        }
    };
    painter.circle_filled(center, radius, fill);
    painter.circle_stroke(center, radius - 0.5, hairline(theme::LAMP_RING));
}

/// A coverage table cell: its lamp and its state, with a not-run cell's reason as the tooltip.
pub fn coverage_cell(ui: &mut Ui, cell: &Cell) -> Response {
    let p = theme::palette(ui.ctx());
    let (lamp, label, color) = match cell {
        Cell::Run => (Lamp::Run, "run", p.ink),
        Cell::Sampled => (Lamp::Sampled, "sampled", p.ink),
        Cell::NotRun { .. } => (Lamp::NotRun, "not run", p.muted),
    };
    let response = ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), Sense::hover());
        paint_lamp(ui, rect.center(), lamp);
        text(ui, label, Type::Cell, color);
    })
    .response;
    match cell {
        Cell::NotRun { reason } => response.on_hover_text(reason.as_str()),
        _ => response,
    }
}

/// A grade's glyph, drawn into a 16 px square from the page's SVG. `ink` stands for the SVG's
/// `currentColor`, which only the silent glyph uses.
pub fn paint_glyph(ui: &Ui, rect: Rect, grade: Grade, ink: Color32) {
    let p = theme::palette(ui.ctx());
    let painter = ui.painter();
    let scale = rect.width() / 16.0;
    let at = |x: f32, y: f32| rect.min + egui::vec2(x, y) * scale;
    let color = grades::color(grade, p);
    // The SVG squares' rx="2", scaled with the glyph.
    let glyph_corner = CornerRadius::same((2.0 * scale).round() as u8);
    match grade {
        Grade::Healed => {
            painter.circle_filled(at(8.0, 8.0), 6.0 * scale, color);
        }
        Grade::RefusedWorkingRemedy => {
            painter.circle_stroke(at(8.0, 8.0), 5.0 * scale, Stroke::new(2.0 * scale, color));
            painter.circle_filled(at(8.0, 8.0), 2.0 * scale, color);
        }
        Grade::RefusedNoRemedy => {
            let points = vec![at(8.0, 2.2), at(14.0, 13.4), at(2.0, 13.4)];
            painter.add(egui::Shape::closed_line(points, Stroke::new(2.0 * scale, color)));
        }
        Grade::DataLost => {
            let square = Rect::from_min_max(at(2.0, 2.0), at(14.0, 14.0));
            painter.rect_filled(square, glyph_corner, color);
            let cross = Stroke::new(1.8 * scale, p.panel);
            painter.line_segment([at(5.0, 5.0), at(11.0, 11.0)], cross);
            painter.line_segment([at(11.0, 5.0), at(5.0, 11.0)], cross);
        }
        Grade::Silent => {
            let square = Rect::from_min_max(at(2.0, 2.0), at(14.0, 14.0));
            painter.rect_filled(square, glyph_corner, ink);
            painter.line_segment([at(8.0, 4.5), at(8.0, 9.0)], Stroke::new(2.0 * scale, color));
            painter.circle_filled(at(8.0, 11.6), 1.2 * scale, color);
        }
    }
}

/// A grade chip: glyph and name on a sunk pad. Silent's chip is latched, an inverted plate.
pub fn grade_chip(ui: &mut Ui, grade: Grade) -> Response {
    let p = theme::palette(ui.ctx());
    let latched = grades::latched(grade);
    let (fill, ink) = if latched { (p.g_silent, p.g_silent_ink) } else { (p.pad, p.ink) };
    let style = if latched { Type::LatchedChip } else { Type::Chip };
    let galley = ui.painter().layout_job(job(grades::name(grade), style, ink));
    let size = egui::vec2(7.0 + 16.0 + 7.0 + galley.size().x + 9.0, 16.0 + 2.0 * 5.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        if latched {
            ui.painter().rect_filled(rect, CornerRadius::same(theme::CHIP_RADIUS), fill);
        } else {
            ui.painter().extend(sunk(ui.ctx(), rect, fill, theme::CHIP_RADIUS));
        }
        let glyph = Rect::from_min_size(Pos2::new(rect.left() + 7.0, rect.center().y - 8.0), egui::vec2(16.0, 16.0));
        paint_glyph(ui, glyph, grade, ink);
        let text_pos = Pos2::new(glyph.right() + 7.0, rect.center().y - galley.size().y / 2.0);
        ui.painter().galley(text_pos, galley, ink);
    }
    response
}

/// One LED of the run meter: a block queued, running, passed or failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Led {
    Queued,
    Running,
    Passed,
    Failed,
}

/// The LED run meter: the lane's label and count over one LED per block, 40 to a row.
pub fn led_meter(ui: &mut Ui, label: &str, leds: &[Led]) -> Response {
    let p = theme::palette(ui.ctx());
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        let done = leds.iter().filter(|led| matches!(led, Led::Passed | Led::Failed)).count();
        ui.horizontal(|ui| {
            engraved(ui, label);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                text(ui, &format!("{} / {}", done, leds.len()), Type::Data, p.ink);
            });
        });
        let per_row = 40usize;
        let rows = leds.len().div_ceil(per_row).max(1);
        let padding = 5.0;
        let gap = 2.0;
        let width = ui.available_width();
        let height = 2.0 * padding + rows as f32 * 12.0 + (rows as f32 - 1.0) * gap;
        let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), Sense::hover());
        ui.painter().extend(sunk(ui.ctx(), rect, p.well, theme::CHIP_RADIUS));
        let cell = (width - 2.0 * padding - (per_row as f32 - 1.0) * gap) / per_row as f32;
        for (index, led) in leds.iter().enumerate() {
            let (row, column) = (index / per_row, index % per_row);
            let min = rect.min + egui::vec2(padding + column as f32 * (cell + gap), padding + row as f32 * (12.0 + gap));
            let color = match led {
                Led::Queued => p.lamp_off,
                Led::Running => p.accent,
                Led::Passed => p.g_healed,
                Led::Failed => p.g_lost,
            };
            ui.painter().rect_filled(Rect::from_min_size(min, egui::vec2(cell, 12.0)), CornerRadius::same(theme::LED_RADIUS), color);
        }
        response
    })
    .inner
}

/// A readout: an engraved label over a 22 px mono value and its unit, in a well.
pub fn readout(ui: &mut Ui, label: &str, value: &str, unit: &str) -> Response {
    let p = theme::palette(ui.ctx());
    in_well(ui, egui::Margin::symmetric(12, 10), |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = U;
        engraved(ui, label);
        ui.horizontal(|ui| {
            text(ui, value, Type::Readout, p.ink);
            if !unit.is_empty() {
                text(ui, unit, Type::ReadoutUnit, p.muted);
            }
        });
    })
    .response
}

/// The step screen's inks: the page's `.crt` text and its `.d`, `.r` and `.a` spans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ink {
    /// What the tool printed.
    Plain,
    /// A command the scenario ran, and a refusal's hint.
    Dim,
    /// A refusal.
    Red,
    /// What the tool did about it.
    Amber,
}

/// A line of the step screen: spans of text, each in its ink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenLine(pub Vec<(Ink, String)>);

impl ScreenLine {
    /// A line in one ink.
    pub fn new(ink: Ink, text: impl Into<String>) -> ScreenLine {
        ScreenLine(vec![(ink, text.into())])
    }

    /// A command the scenario ran, `$ <command>`, dim.
    pub fn command(command: &str) -> ScreenLine {
        ScreenLine::new(Ink::Dim, format!("$ {command}"))
    }

    /// This line with `text` added after it, in `ink`.
    pub fn then(mut self, ink: Ink, text: impl Into<String>) -> ScreenLine {
        self.0.push((ink, text.into()));
        self
    }
}

/// The step screen: a fixed dark CRT in both themes, one row per line.
pub fn screen(ui: &mut Ui, lines: &[ScreenLine]) -> Response {
    let p = theme::palette(ui.ctx());
    let frame = egui::Frame::new().fill(p.crt_bg).corner_radius(CornerRadius::same(theme::R_S))
        .inner_margin(egui::Margin::symmetric(14, 12)).stroke(Stroke::new(3.0_f32, p.pad_lo));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 12.0 * 0.6;
        egui::ScrollArea::horizontal().show(ui, |ui| {
            for line in lines {
                let mut row = LayoutJob::default();
                for (ink, text) in &line.0 {
                    let color = match ink {
                        Ink::Plain => p.crt_ink,
                        Ink::Dim => p.crt_dim,
                        Ink::Red => p.crt_red,
                        Ink::Amber => p.crt_amber,
                    };
                    row.append(text, 0.0, Type::Screen.format(color));
                }
                if row.is_empty() {
                    // A blank line keeps its height, as the page's `white-space: pre` does.
                    row.append("", 0.0, Type::Screen.format(p.crt_ink));
                }
                ui.add(egui::Label::new(row).extend());
            }
        });
    })
    .response
}

/// The run's tally: one line per grade, its glyph, name and count.
pub fn tally(ui: &mut Ui, lines: &[(Grade, usize)]) -> Response {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = theme::TALLY_GAP;
        for (grade, count) in lines {
            tally_line(ui, *grade, *count);
        }
    })
    .response
}

/// One tally line: the grade's glyph, its name, and its count at the right.
fn tally_line(ui: &mut Ui, grade: Grade, count: usize) -> Response {
    let p: &Palette = theme::palette(ui.ctx());
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 16.0), Sense::hover());
        paint_glyph(ui, Rect::from_center_size(rect.center(), egui::vec2(16.0, 16.0)), grade, p.ink);
        text(ui, grades::name(grade), Type::Tally, p.ink);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            text(ui, &count.to_string(), Type::TallyCount, p.ink);
        });
    })
    .response
}
