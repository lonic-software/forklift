//! How the page shows the library's grades: their names, their colour token, and the order the
//! results sort in. Which grade a row has, and whether it fails, is the library's (oracle::Grade);
//! nothing here decides either.

use eframe::egui::Color32;
use proof_load::oracle::Grade;

use crate::theme::Palette;

/// Worst first, as the results table sorts: silent, data lost, no remedy, remedy works, healed.
pub const WORST_FIRST: [Grade; 5] = [Grade::Silent, Grade::DataLost, Grade::RefusedNoRemedy, Grade::RefusedWorkingRemedy, Grade::Healed];

/// A grade's place in `WORST_FIRST`, 0 for the worst: the library's severity, reversed.
pub fn rank(grade: Grade) -> usize {
    usize::from(Grade::Silent.severity() - grade.severity())
}

/// The name a chip and the tally show.
pub fn name(grade: Grade) -> &'static str {
    match grade {
        Grade::Silent => "Silent",
        Grade::DataLost => "Data lost",
        Grade::RefusedNoRemedy => "Refused, no remedy",
        Grade::RefusedWorkingRemedy => "Refused, remedy works",
        Grade::Healed => "Healed",
    }
}

/// The grade's own colour token (`--g-*`).
pub fn color(grade: Grade, palette: &Palette) -> Color32 {
    match grade {
        Grade::Silent => palette.g_silent,
        Grade::DataLost => palette.g_lost,
        Grade::RefusedNoRemedy => palette.g_noremedy,
        Grade::RefusedWorkingRemedy => palette.g_remedy,
        Grade::Healed => palette.g_healed,
    }
}

/// Silent is the one latched grade: its chip is an inverted plate.
pub fn latched(grade: Grade) -> bool {
    grade == Grade::Silent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worst_first_ranks_in_order_and_holds_every_grade_once() {
        for (index, grade) in WORST_FIRST.into_iter().enumerate() {
            assert_eq!(rank(grade), index, "{grade:?}");
        }
        // The library's failing grades are exactly the first three: worse than every passing one.
        let failing: Vec<bool> = WORST_FIRST.iter().map(|grade| grade.is_failure()).collect();
        assert_eq!(failing, [true, true, true, false, false]);
    }

    #[test]
    fn only_silent_latches() {
        let latched: Vec<Grade> = WORST_FIRST.into_iter().filter(|grade| latched(*grade)).collect();
        assert_eq!(latched, [Grade::Silent]);
    }
}
