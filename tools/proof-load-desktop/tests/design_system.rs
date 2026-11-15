//! The desktop against its design system, docs/proof-load-design-system.html: every token of the
//! page's light and dark `:root` blocks has the same value in src/theme.rs, the theme has no token
//! the page lacks, and no file in src/ but theme.rs writes a colour or a font size.
//!
//! The page's tokens are shared with pult-desktop's, and nothing here reads that repository: a
//! change there does not fail this test. The page records which pult-desktop commit it was last
//! compared with.

use std::collections::BTreeMap;

use proof_load_desktop::theme::{self, Palette, DARK, LIGHT};

const PAGE: &str = include_str!("../../../docs/proof-load-design-system.html");

/// Tokens that are not colours, and how the theme holds each.
const NOT_COLOURS: [&str; 7] = ["--r-s", "--r-l", "--u", "--sans", "--mono", "--shadow-raise", "--shadow-sink"];

/// The custom properties of the block that opens at `opening`, up to its first `}`, comments
/// removed.
fn block(opening: &str) -> BTreeMap<String, String> {
    let start = PAGE.find(opening).unwrap_or_else(|| panic!("the page has no block opening {opening:?}")) + opening.len();
    let body = &PAGE[start..start + PAGE[start..].find('}').expect("the block closes")];
    let mut without_comments = String::new();
    let mut rest = body;
    while let Some(open) = rest.find("/*") {
        without_comments.push_str(&rest[..open]);
        let close = rest[open..].find("*/").expect("the comment closes");
        rest = &rest[open + close + 2..];
    }
    without_comments.push_str(rest);
    let mut tokens = BTreeMap::new();
    for declaration in without_comments.split(';') {
        let Some((name, value)) = declaration.split_once(':') else { continue };
        let name = name.trim();
        if name.starts_with("--") {
            assert!(tokens.insert(name.to_string(), value.trim().to_string()).is_none(), "{name} is set twice in one block");
        }
    }
    tokens
}

/// The page's tokens in light, and in dark: light's with the dark block over them.
fn page_themes() -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let light = block(":root {");
    let dark_by_preference = block(r#":root:not([data-theme="light"]) {"#);
    let dark_by_switch = block(r#":root[data-theme="dark"] {"#);
    assert_eq!(dark_by_preference, dark_by_switch, "the page's two dark blocks differ");
    for name in dark_by_preference.keys() {
        assert!(light.contains_key(name), "{name} is set in dark only");
    }
    let mut dark = light.clone();
    dark.extend(dark_by_preference);
    (light, dark)
}

fn hex(colour: eframe::egui::Color32) -> String {
    assert_eq!(colour.a(), 255, "a theme colour is opaque");
    format!("#{:02x}{:02x}{:02x}", colour.r(), colour.g(), colour.b())
}

fn first_family(stack: &str) -> &str {
    stack.split(',').next().unwrap().trim().trim_matches('"')
}

fn compare(which: &str, page: &BTreeMap<String, String>, palette: &Palette) {
    let mut mismatches = Vec::new();
    let colours: BTreeMap<&str, String> = palette.tokens().into_iter().map(|(name, colour)| (name, hex(colour))).collect();
    for (name, value) in page {
        if NOT_COLOURS.contains(&name.as_str()) {
            continue;
        }
        match colours.get(name.as_str()) {
            Some(ours) if *ours == value.to_lowercase() => {}
            Some(ours) => mismatches.push(format!("{name}: page {value}, theme {ours}")),
            None => mismatches.push(format!("{name}: on the page, not in the theme")),
        }
    }
    for name in colours.keys() {
        if !page.contains_key(*name) {
            mismatches.push(format!("{name}: in the theme, not on the page"));
        }
    }
    assert!(mismatches.is_empty(), "{which} differs from the page:\n{}", mismatches.join("\n"));
}

#[test]
fn every_colour_token_matches_the_page_in_both_themes() {
    let (light, dark) = page_themes();
    compare("light", &light, &LIGHT);
    compare("dark", &dark, &DARK);
}

#[test]
fn the_shape_and_type_tokens_match_the_page() {
    let (light, _) = page_themes();
    assert_eq!(light["--r-s"], format!("{}px", theme::R_S));
    assert_eq!(light["--r-l"], format!("{}px", theme::R_L));
    assert_eq!(light["--u"], format!("{}px", theme::U));
    assert_eq!(first_family(&light["--sans"]), theme::SANS_FAMILY);
    assert_eq!(first_family(&light["--mono"]), theme::MONO_FAMILY);
    // The shadows are drawn as a highlight and a shade stroke (widgets::raised and widgets::sunk),
    // from the pad tokens the colour test compares; their CSS values have no egui counterpart.
    for name in NOT_COLOURS {
        assert!(light.contains_key(name), "{name} is no longer on the page; drop it from NOT_COLOURS");
    }
}

/// The lines of `source` that write a colour or a font size, which only theme.rs may: a colour
/// constructor, constant, conversion or derivation, a `FontId` or `TextStyle` written out, or
/// egui's own sizing, weighting and face changes of text (`RichText::size`, `strong`, `code`,
/// `family` and the rest), whitespace ignored. A `galley.size()` is a measurement, not a size
/// written, and a `face.family()` a getter: a `size` or `family` called with nothing is allowed.
fn literals(source: &str) -> Vec<String> {
    const WRITES: [&str; 22] = [
        "Color32::", "Color32 as", "Rgba", "Hsva", "Hsla", "from_rgb", "from_gray", "from_hex", "from_srgb",
        "gamma_multiply", "linear_multiply", "lerp_to_gamma", "FontId", "TextStyle::", ".strong()",
        ".heading()", ".small()", ".monospace()", ".weak()", ".text_style(", ".code()",
        ".italics()",
    ];
    source.lines().enumerate()
        .filter(|(_, line)| {
            let line: String = line.chars().filter(|c| !c.is_whitespace()).collect();
            WRITES.iter().any(|write| line.contains(&write.replace(' ', ""))) || ["size", "family"].iter().any(|setter| {
                line.replace(&format!(".{setter}()"), "").contains(&format!(".{setter}("))
            })
        })
        .map(|(index, line)| format!("{}: {}", index + 1, line.trim())).collect()
}

#[test]
fn no_file_but_the_theme_writes_a_colour_or_a_font_size() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut checked = Vec::new();
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        assert!(path.extension().is_some_and(|extension| extension == "rs"), "src/ holds only .rs files, so none escapes this scan: {}", path.display());
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name == "theme.rs" {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        found.extend(literals(&source).into_iter().map(|line| format!("src/{name}:{line}")));
        checked.push(name);
    }
    checked.sort();
    assert_eq!(checked, ["app.rs", "fonts.rs", "grades.rs", "lib.rs", "main.rs", "widgets.rs"]);
    assert!(found.is_empty(), "colours or font sizes outside theme.rs:\n{}", found.join("\n"));
}

#[test]
fn the_literal_scan_finds_what_it_bans() {
    assert_eq!(literals("let a = 1;\nlet red = Color32::RED;"), ["2: let red = Color32::RED;"]);
    for write in ["FontId::new(9.0, family)", "FontId { size: 9.0, family }", "RichText::new(x).size(20.0)", "RichText::new(x).strong()",
        "ink.gamma_multiply(0.45)", "RichText::new(x).size (20.0)", "RichText::new(x).code()", "RichText::new(x).family(f)", "use egui::Color32 as C;", "Rgba::from_rgb(1.0, 0.0, 0.0)", "ecolor::Hsva::new(0.0, 1.0, 1.0, 1.0)"] {
        assert_eq!(literals(write).len(), 1, "the scan misses {write}");
    }
    assert!(literals("let width = galley.size().x;").is_empty(), "a measured size is not a size written");
    let theme = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/theme.rs")).unwrap();
    assert!(!literals(&theme).is_empty(), "theme.rs writes its colours in a form the scan sees");
}
