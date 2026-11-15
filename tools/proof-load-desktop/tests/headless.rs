//! The window drawn into a bare egui context, with no window opened: it lays out in both themes,
//! and every colour it paints is a token of that theme's palette, a disabled or hovered form of
//! one the page defines, or the lamp's ring, so neither egui's own defaults nor a widget's
//! arithmetic puts a colour on screen that the design system does not have.

use std::collections::BTreeSet;

use eframe::egui::{self, epaint, Color32};
use proof_load_desktop::app::App;
use proof_load::report::Cell;
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

fn painted(theme: egui::Theme) -> BTreeSet<[u8; 4]> {
    let mut app = App::default();
    painted_by(theme, |ctx| app.show(ctx))
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
    for token in [palette.bg, palette.panel, palette.ink, palette.accent, palette.g_silent, palette.crt_bg] {
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
