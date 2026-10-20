use serde::Serialize;
use forklift_core::util::alias_utils::AliasCheck;
use forklift_core::util::inventory_utils;
use forklift_core::util::path_utils::WarehousePath;
use crate::output::{self, CommandOutput};

/// Handle the load command.
/// This command adds the given file or directory to its corresponding inventory.
///
/// # Arguments
/// * `target` - The path of the file or directory to load.
///
/// # Returns
/// * `Ok(())`      - If the command was handled successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub async fn handle_command(target: &str) -> Result<(), String> {
    let path = WarehousePath::from_user_input(target)?;

    // In a scoped bay an out-of-scope path is not materialized, so staging it would be a
    // confusing "not in the inventory" (or a silent no-op) — refuse it clearly instead (§7.6).
    // The scope is judged by the path the volume opens, so a case or normalization alias of an
    // in-scope path meets the spelling refusal, which names the listed path, not this one.
    let opened = AliasCheck::new().resolve(path.as_key())?;
    crate::commands::scope::ensure_path_in_scope(opened.as_deref().unwrap_or(path.as_key()))?;

    inventory_utils::add_changes_to_inventory(&path).await?;

    output::emit(&Loaded { path: path.as_key().to_string() });

    Ok(())
}

/// The path a `load` staged. Human output stays silent (as it always has); `--json`
/// still gets a confirmation envelope so a program sees a result.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Loaded {
    path: String,
}

impl CommandOutput for Loaded {
    fn render_human(&self) {}
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<Loaded>(),
    ]
}

/// The alias test a named load chooses its remedy by. It reads paths against the process working
/// directory, so each test holds it through a working-tree warehouse.
#[cfg(all(test, unix))]
mod tests {
    use super::AliasCheck;
    use crate::test_support::Warehouse;

    /// Whether the working directory's volume opens a name spelled in another case.
    fn folds_case() -> bool {
        std::fs::write("case-probe", "").unwrap();
        let folds = std::path::Path::new("CASE-PROBE").exists();
        std::fs::remove_file("case-probe").unwrap();
        folds
    }

    #[test]
    fn spellings_that_open_one_listed_entry_are_one_entry() {
        let _warehouse = Warehouse::with_working_tree("alias-one-listed");
        let folds = folds_case();
        std::fs::create_dir("Docs").unwrap();
        std::fs::write("Docs/README", "r\n").unwrap();
        let mut check = AliasCheck::new();

        assert_eq!(check.resolve("Docs/README").unwrap().as_deref(), Some("Docs/README"));
        assert_eq!(check.resolve("docs/readme").unwrap().as_deref(), folds.then_some("Docs/README"),
            "an aliased directory does not end the walk");
        assert_eq!(check.one_entry("Docs/README", "Docs/readme").unwrap(), folds, "a listed and an unlisted spelling");
        assert_eq!(check.one_entry("Docs/readme", "Docs/Readme").unwrap(), folds, "two unlisted spellings of a third");
        assert_eq!(check.one_entry("docs/readme", "Docs/README").unwrap(), folds, "a pair under an aliased directory");
        assert!(!check.one_entry("Docs/README", "Docs/other").unwrap(), "names that do not fold together");
    }

    #[test]
    fn spellings_that_both_open_nothing_are_one_entry_when_the_volume_folds_them() {
        let _warehouse = Warehouse::with_working_tree("alias-both-gone");
        let folds = folds_case();
        std::fs::create_dir("d").unwrap();
        // Where the old directory spelling is now a file, both paths meet "not a directory".
        std::fs::write("docs", "x\n").unwrap();
        let mut check = AliasCheck::new();

        assert_eq!(check.resolve("d/README").unwrap(), None);
        assert_eq!(check.one_entry("d/README", "d/readme").unwrap(), folds);
        assert_eq!(check.one_entry("gone/README", "GONE/README").unwrap(), folds, "a missing parent is probed above it");
        assert_eq!(check.resolve("Docs/a").unwrap(), None, "\"not a directory\" is an absence, not an error");
        assert_eq!(check.one_entry("Docs/a", "docs/a").unwrap(), folds);
    }

    #[test]
    fn names_listed_under_their_own_spelling_are_two_entries() {
        let _warehouse = Warehouse::with_working_tree("alias-hard-links");
        if folds_case() {
            return;
        }
        std::fs::write("README", "r\n").unwrap();
        std::fs::hard_link("README", "readme").unwrap();
        let mut check = AliasCheck::new();

        assert!(!check.one_entry("README", "readme").unwrap(), "hard links listed under both names are two entries");
    }
}
