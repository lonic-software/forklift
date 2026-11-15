//! IBM Plex Sans and Mono, embedded in the binary so every OS shows the same type. The files and
//! their licence (SIL Open Font License 1.1) are in fonts/, from IBM/plex's releases
//! @ibm/plex-sans@1.1.0 and @ibm/plex-mono@1.1.0.

use std::sync::Arc;

use eframe::egui::{FontData, FontDefinitions};

use crate::theme::Face;

fn bytes(face: Face) -> &'static [u8] {
    match face {
        Face::Sans => include_bytes!("../fonts/IBMPlexSans-Regular.ttf"),
        Face::SansMedium => include_bytes!("../fonts/IBMPlexSans-Medium.ttf"),
        Face::SansSemiBold => include_bytes!("../fonts/IBMPlexSans-SemiBold.ttf"),
        Face::Mono => include_bytes!("../fonts/IBMPlexMono-Regular.ttf"),
        Face::MonoMedium => include_bytes!("../fonts/IBMPlexMono-Medium.ttf"),
        Face::MonoSemiBold => include_bytes!("../fonts/IBMPlexMono-SemiBold.ttf"),
    }
}

/// egui's default fonts with each Plex face in front of its family, so a character Plex lacks
/// still falls back to egui's own fonts.
pub fn definitions() -> FontDefinitions {
    let mut definitions = FontDefinitions::default();
    let fallback = |definitions: &FontDefinitions, mono: bool| {
        let family = if mono { eframe::egui::FontFamily::Monospace } else { eframe::egui::FontFamily::Proportional };
        definitions.families.get(&family).cloned().unwrap_or_default()
    };
    let sans_fallback = fallback(&definitions, false);
    let mono_fallback = fallback(&definitions, true);
    for face in Face::ALL {
        definitions.font_data.insert(face.font_name().to_string(), Arc::new(FontData::from_static(bytes(face))));
        let mono = matches!(face, Face::Mono | Face::MonoMedium | Face::MonoSemiBold);
        let mut chain = vec![face.font_name().to_string()];
        chain.extend(if mono { mono_fallback.clone() } else { sans_fallback.clone() });
        definitions.families.insert(face.family(), chain);
    }
    definitions
}
