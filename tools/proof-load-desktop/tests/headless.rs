//! The window drawn into a bare egui context, with no window opened: it lays out in both themes,
//! and every colour it paints is a token of that theme's palette, a disabled or hovered form of
//! one the page defines, or the lamp's ring, so neither egui's own defaults nor a widget's
//! arithmetic puts a colour on screen that the design system does not have.

use std::collections::BTreeSet;

use eframe::egui::{self, epaint, Color32};
use proof_load::report::{Category, Cell};
use proof_load::run::RunError;
use proof_load_desktop::app::{App, Screen};
use proof_load_desktop::results::Results;
use proof_load_desktop::run_state::Event;
use proof_load_desktop::{theme, widgets};

/// Every colour the window's last frame puts on screen: the frame is tessellated as eframe would,
/// so a text's fallback and opacity, a mesh and egui's own decoration are all read as painted.
/// Anti-aliasing adds an outer ring of transparent vertices, which is dropped, not partial alphas.
fn painted_by(theme: egui::Theme, mut show: impl FnMut(&egui::Context)) -> BTreeSet<[u8; 4]> {
    let ctx = egui::Context::default();
    proof_load_desktop::install(&ctx);
    ctx.set_theme(theme);
    let input = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1120.0, 2400.0))),
        ..egui::RawInput::default()
    };
    // egui lays a frame out again when a size it measured changes; a few frames settle it.
    let mut output = ctx.run(input(), |ctx| show(ctx));
    for _ in 0..2 {
        output = ctx.run(input(), |ctx| show(ctx));
    }
    let mut found = BTreeSet::new();
    for clipped in ctx.tessellate(output.shapes, output.pixels_per_point) {
        match clipped.primitive {
            epaint::Primitive::Mesh(mesh) => found.extend(mesh.vertices.iter().map(|vertex| vertex.color.to_array())),
            epaint::Primitive::Callback(_) => panic!("the window paints a callback, whose colours this test cannot read"),
        }
    }
    found.remove(&Color32::TRANSPARENT.to_array());
    found
}

/// The report a real run of the signed scenario wrote, with two rows' details beside it.
fn fixture() -> Results {
    Results::open(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/signed")).expect("the fixture reads")
}

/// Each screen as a run leaves it: the Run screen with its fields in error and a meter part way
/// through, the Results screen on a row whose steps it shows and on one whose details are gone,
/// and the Coverage screen.
fn screens() -> Vec<(&'static str, App)> {
    let mut run = App::default();
    for event in [
        Event::Lane(Category::DamagedFiles, Some(5)),
        Event::Started(Category::DamagedFiles, "a".into()),
        Event::Finished(Category::DamagedFiles, "a".into(), false),
        Event::Started(Category::DamagedFiles, "b".into()),
        Event::Finished(Category::DamagedFiles, "b".into(), true),
        Event::Started(Category::DamagedFiles, "c".into()),
        Event::Message("built the warehouse".into()),
        Event::Done(Err(RunError::Refused("the source tree has no Cargo.toml".into()))),
    ] {
        run.meter.apply(event);
    }
    let mut steps = App::showing(fixture());
    assert!(steps.select("signed trust FlipByte"));
    let gone = App::showing(fixture());
    let mut coverage = App::showing(fixture());
    coverage.screen = Screen::Coverage;
    vec![("run", run), ("results", steps), ("results without details", gone), ("coverage", coverage)]
}

/// The texts the app's last frame draws, each with its first section's colour.
fn texts(theme: egui::Theme, app: &mut App) -> Vec<(String, egui::Color32)> {
    texts_at(theme, app, 1120.0)
}

/// `texts` in a window `width` wide.
fn texts_at(theme: egui::Theme, app: &mut App, width: f32) -> Vec<(String, egui::Color32)> {
    shown_texts(theme, app, width).into_iter().map(|(text, colour, _)| (text, colour)).collect()
}

/// Each text drawn, its colour, and how wide a part of it its clip leaves on screen.
fn shown_texts(theme: egui::Theme, app: &mut App, width: f32) -> Vec<(String, egui::Color32, f32)> {
    let ctx = egui::Context::default();
    proof_load_desktop::install(&ctx);
    ctx.set_theme(theme);
    let input = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 2400.0))),
        ..egui::RawInput::default()
    };
    let mut output = ctx.run(input(), |ctx| app.show(ctx));
    for _ in 0..2 {
        output = ctx.run(input(), |ctx| app.show(ctx));
    }
    fn walk(shape: &epaint::Shape, clip: egui::Rect, found: &mut Vec<(String, egui::Color32, f32)>) {
        match shape {
            epaint::Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, clip, found)),
            epaint::Shape::Text(text) => {
                let colour = text.galley.job.sections.first().map_or(text.fallback_color, |section| section.format.color);
                let shown = text.visual_bounding_rect().intersect(clip).width().max(0.0);
                found.push((text.galley.text().to_string(), colour, shown));
            }
            _ => {}
        }
    }
    let mut found = Vec::new();
    for clipped in &output.shapes {
        walk(&clipped.shape, clipped.clip_rect, &mut found);
    }
    found
}

fn painted(theme: egui::Theme) -> BTreeSet<[u8; 4]> {
    let mut all = BTreeSet::new();
    for (_, mut app) in screens() {
        all.extend(painted_by(theme, |ctx| app.show(ctx)));
    }
    all
}

/// What one widget paints, alone in a panel.
fn painted_by_widget(theme: egui::Theme, mut add: impl FnMut(&mut egui::Ui)) -> BTreeSet<[u8; 4]> {
    painted_by(theme, |ctx| {
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| add(ui));
    })
}

fn check(which: egui::Theme) {
    let palette = theme::palette_of(which);
    let allowed: BTreeSet<[u8; 4]> = palette.tokens().into_iter()
        .flat_map(|(_, colour)| [colour, theme::disabled(colour)])
        .chain([theme::LAMP_RING, theme::hovered_primary(palette.accent)])
        .map(|colour| colour.to_array()).collect();
    let painted = painted(which);
    assert!(painted.len() >= 10, "the {which:?} window painted almost nothing: {painted:?}");
    let named = |colour: &[u8; 4]| {
        palette.tokens().into_iter().find(|(_, token)| token.to_array() == *colour).map(|(name, _)| name)
    };
    let strays: Vec<String> = painted.iter().filter(|colour| !allowed.contains(*colour)).map(|colour| format!("{colour:?}")).collect();
    assert!(strays.is_empty(), "{which:?} paints colours that are not tokens: {}", strays.join(", "));
    // The window uses the theme's own surfaces and a grade colour, not only some of its tokens.
    for token in [palette.bg, palette.panel, palette.ink, palette.accent, palette.g_lost, palette.g_healed, palette.crt_bg] {
        assert!(painted.contains(&token.to_array()), "{which:?} never paints {:?}", named(&token.to_array()));
    }
}

#[test]
fn the_light_window_paints_only_light_tokens() {
    check(egui::Theme::Light);
}

#[test]
fn the_dark_window_paints_only_dark_tokens() {
    check(egui::Theme::Dark);
}

#[test]
fn a_disabled_button_draws_its_text_at_45_percent() {
    for which in [egui::Theme::Light, egui::Theme::Dark] {
        let palette = theme::palette_of(which);
        let painted = painted_by_widget(which, |ui| {
            widgets::button(ui, "Open report", widgets::Button::Default, false);
        });
        assert!(painted.contains(&theme::disabled(palette.ink).to_array()), "{which:?}: {painted:?}");
        assert!(!painted.contains(&palette.ink.to_array()), "{which:?} draws a disabled label at full ink");
    }
}

#[test]
fn an_unpressed_segment_is_muted_and_the_pressed_one_is_ink() {
    for which in [egui::Theme::Light, egui::Theme::Dark] {
        let palette = theme::palette_of(which);
        let painted = painted_by_widget(which, |ui| {
            widgets::segmented(ui, &mut 0, &["Failures", "All"]);
        });
        assert!(painted.contains(&palette.ink.to_array()), "{which:?} has no pressed segment in ink");
        assert!(painted.contains(&palette.muted.to_array()), "{which:?} has no unpressed segment muted");
    }
}

#[test]
fn a_raised_pad_has_a_bottom_shade_in_light_only() {
    for (which, shaded) in [(egui::Theme::Light, true), (egui::Theme::Dark, false)] {
        let palette = theme::palette_of(which);
        let painted = painted_by_widget(which, |ui| {
            widgets::button(ui, "Open report", widgets::Button::Default, true);
        });
        assert!(painted.contains(&palette.pad_hi.to_array()), "{which:?} has no top highlight");
        assert_eq!(painted.contains(&palette.pad_lo.to_array()), shaded, "{which:?}'s bottom shade");
    }
}

#[test]
fn a_lit_lamp_has_its_ring_and_a_sampled_one_does_not() {
    let ring = theme::LAMP_RING.to_array();
    let lamp = |cell: Cell| painted_by_widget(egui::Theme::Light, move |ui| {
        widgets::coverage_cell(ui, &cell);
    });
    assert!(lamp(Cell::Run).contains(&ring));
    assert!(!lamp(Cell::Sampled).contains(&ring));
}

#[test]
fn a_hovered_primary_button_brightens_its_accent() {
    let palette = theme::palette_of(egui::Theme::Light);
    let ctx = egui::Context::default();
    proof_load_desktop::install(&ctx);
    ctx.set_theme(egui::Theme::Light);
    let mut fill = None;
    for _ in 0..3 {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 200.0))),
            events: vec![egui::Event::PointerMoved(egui::pos2(20.0, 16.0))],
            ..egui::RawInput::default()
        };
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| {
                widgets::button(ui, "Run scenarios", widgets::Button::Primary, true);
            });
        });
        fill = ctx.tessellate(output.shapes, output.pixels_per_point).into_iter().find_map(|clipped| match clipped.primitive {
            epaint::Primitive::Mesh(mesh) => mesh.vertices.first().map(|vertex| vertex.color),
            epaint::Primitive::Callback(_) => None,
        });
    }
    assert_ne!(theme::hovered_primary(palette.accent), palette.accent);
    assert_eq!(fill, Some(theme::hovered_primary(palette.accent)));
}

#[test]
fn a_blank_step_screen_line_keeps_its_height() {
    let height = |lines: Vec<widgets::ScreenLine>| {
        let ctx = egui::Context::default();
        proof_load_desktop::install(&ctx);
        let mut height = 0.0;
        for _ in 0..2 {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| height = widgets::screen(ui, &lines).rect.height());
            });
        }
        height
    };
    let line = || widgets::ScreenLine::command("forklift heal");
    let two = height(vec![line(), line()]);
    let with_blank = height(vec![line(), widgets::ScreenLine(Vec::new()), line()]);
    assert!(with_blank > two + 10.0, "a blank line between two adds {} px", with_blank - two);
}

/// Each screen, alone, paints what it exists to show: the meter's lit LEDs and a refusal on the Run
/// screen, the selected row's step screen, the refusal of steps whose details are gone, and the
/// coverage lamps.
///
/// Revert: show the refusal of unreadable details in muted ink → it is not drawn in `--g-lost`.
#[test]
fn each_screen_paints_what_it_shows() {
    let palette = theme::palette_of(egui::Theme::Light);
    for (name, mut app) in screens() {
        let painted = painted_by(egui::Theme::Light, |ctx| app.show(ctx));
        let wanted: &[egui::Color32] = match name {
            "run" => &[palette.g_healed, palette.g_lost, palette.accent, palette.lamp_off],
            "results" => &[palette.crt_bg, palette.crt_red, palette.accent, palette.well],
            "results without details" => &[palette.g_lost],
            _ => &[palette.g_healed, palette.notrun],
        };
        for token in wanted {
            assert!(painted.contains(&token.to_array()), "the {name} screen never paints {token:?}");
        }
        if name == "results without details" {
            assert!(!painted.contains(&palette.crt_bg.to_array()), "a row whose details are gone shows a step screen");
            let refusal = texts(egui::Theme::Light, &mut app).into_iter().find(|(text, _)| text.starts_with("The steps cannot be shown: "));
            let (text, colour) = refusal.expect("the refusal is drawn");
            assert!(text.contains("cannot read"), "{text}");
            assert_eq!(colour, palette.g_lost, "the refusal is drawn loud");
        }
    }
}

/// A click on a row picks that row, though its cells hold widgets of their own.
///
/// Revert: make the row's interaction before its cells → a cell's chip takes the click.
#[test]
fn a_click_picks_the_row_under_it() {
    let ctx = egui::Context::default();
    proof_load_desktop::install(&ctx);
    let columns = [widgets::Column { heading: "Grade", width: 0.0 }];
    let head = 2.0 * 8.0 + theme::Type::Label.font().size;
    let y = head + 1.5 * theme::ROW_HEIGHT;
    let mut picked = None;
    for event in [
        vec![egui::Event::PointerMoved(egui::pos2(30.0, y))],
        vec![egui::Event::PointerButton { pos: egui::pos2(30.0, y), button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() }],
        vec![egui::Event::PointerButton { pos: egui::pos2(30.0, y), button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() }],
    ] {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0))),
            events: event,
            ..egui::RawInput::default()
        };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| {
                if let Some(row) = widgets::table(ui, &columns, 3, true, None, |ui, _, _| {
                    widgets::grade_chip(ui, proof_load::oracle::Grade::Healed);
                }) {
                    picked = Some(row);
                }
            });
        });
    }
    assert_eq!(picked, Some(1));
}

/// At the window's narrowest the results table keeps its Scenario column and scrolls sideways,
/// rather than squeezing scenario names to nothing.
///
/// Revert: let the flexible column shrink to 0 → no scenario name is drawn at 760 px.
#[test]
fn a_narrow_window_keeps_the_scenario_names() {
    let mut app = App::showing(fixture());
    let names: Vec<(String, f32)> = shown_texts(egui::Theme::Light, &mut app, 760.0).into_iter()
        .filter(|(text, _, _)| text.starts_with("signed ")).map(|(text, _, shown)| (text, shown)).collect();
    assert!(!names.is_empty() && names.iter().all(|(_, shown)| *shown >= 100.0), "scenario names squeezed at 760 px: {names:?}");
}
