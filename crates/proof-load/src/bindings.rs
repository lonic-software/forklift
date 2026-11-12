//! The damage bindings: for every census name, the scenario that creates the path and the damage
//! kinds applied to it, or an exemption with its reason. A run checks each binding's scenario
//! creates its name, and fails on a census name neither list covers.
//!
//! Every binding here names a path a run of its scenario was seen to create; a name no scenario of
//! this build creates is left uncovered, so the run says so, rather than bound to a scenario that
//! would only fail its own check.

use std::collections::BTreeMap;

use crate::census::Census;
use crate::damage::{self, Binding, Exemption, Kind};
use crate::scenario::Scenario;

const FILE: &[Kind] = &[Kind::FlipByte, Kind::Truncate, Kind::Zero, Kind::Delete];
/// A lock file left in place between commands: it may be empty, so no byte is there to flip.
const LOCK_FILE: &[Kind] = &[Kind::Truncate, Kind::Delete];
const FOLDER: &[Kind] = &[Kind::Delete];

const fn bound(name: &'static str, kinds: &'static [Kind]) -> Binding {
    bound_in(name, "history", kinds)
}

const fn bound_in(name: &'static str, scenario: &'static str, kinds: &'static [Kind]) -> Binding {
    Binding { name, scenario, kinds }
}

pub const BINDINGS: &[Binding] = &[
    bound("warehouse", FILE),
    bound("format.lock", LOCK_FILE),
    bound("refs.lock", LOCK_FILE),
    bound("store-access.lock", LOCK_FILE),
    bound("config/", FOLDER),
    bound("config/warehouse.toml", FILE),
    bound("config/warehouse.toml.lock", LOCK_FILE),
    bound("objects/", FOLDER),
    bound("objects/<xx>/", FOLDER),
    bound("objects/<xx>/<hash>", FILE),
    bound("pallets/", FOLDER),
    bound("pallets/<name>", FILE),
    bound("graph/", FOLDER),
    bound("graph/<xx>", FILE),
    bound("{bay}inventory/", FOLDER),
    bound("{bay}inventory/metadata", FILE),
    bound("{bay}inventory/inv_/", FOLDER),
    bound("{bay}inventory/inv_/<xx>/", FOLDER),
    bound("{bay}inventory/inv_/<xx>/<hash>/", FOLDER),
    bound("{bay}inventory/inv_/<xx>/<hash>/data", FILE),
    bound("{bay}journal.json", FILE),
    bound("{bay}position", FILE),
    bound_in("trust", "signed", FILE),
    bound_in("meta/", "signed", FOLDER),
    bound_in("meta/<name>", "signed", FILE),
    bound_in("objects/<xx>/<hash>.sig", "signed", FILE),
    bound_in("objects/pack/", "packed", FOLDER),
    bound_in("objects/pack/<name>.pack", "packed", FILE),
    bound_in("objects/pack/<name>.idx", "packed", FILE),
    bound_in("objects/pack/<name>.sigs", "packed", FILE),
    bound_in("{bay}parked", "parked", FILE),
    bound_in("{bay}consolidation", "conflicted", FILE),
    bound_in("{bay}consolidation-skeleton", "conflicted", FILE),
    bound_in("{bay}cherry-pick", "picking", FILE),
    bound_in("held-heads", "lifted", FILE),
    bound_in("bundles/", "bundled", FOLDER),
    bound_in("bundles/latest", "bundled", FILE),
    bound_in("objects/pack/densify-pending", "bundled", FILE),
    bound_in("origin", "cached", FILE),
    bound_in("sealed", "cached", FILE),
    bound_in("origin-move.lock", "moved", LOCK_FILE),
];

const NO_MIRROR_ORACLE: &str = "no mirror oracle: the corpus configures no mirror url, so nothing reads a mirror back";
const INTERRUPTED: &str = "present only while a command runs or after one is interrupted, and removed when it finishes: no finished history holds one to damage, so the kill-point faults grade it";
const SYNC_FAILED: &str = "written only when a directory sync after a rename fails, and cleared by heal: no finished history holds one to damage, so the power-loss and kill-point faults grade it";
const HELD_LOCK: &str = "removed when the command holding it exits, a hard kill leaving it behind: the dead-process lock fault grades it, not a damaged copy";
const PATH_BOUND: &str = "path-bound: a bay records its absolute path, so a run on a copy cannot reach it";

pub const EXEMPTIONS: &[Exemption] = &[
    Exemption { name: "mirror-roots", reason: NO_MIRROR_ORACLE },
    Exemption { name: "mirror.lock", reason: NO_MIRROR_ORACLE },
    Exemption { name: "bays/", reason: PATH_BOUND },
    Exemption { name: "bays/<name>/", reason: PATH_BOUND },
    Exemption { name: "bays/<name>/path", reason: PATH_BOUND },
    Exemption { name: "redirect", reason: PATH_BOUND },
    Exemption { name: "bundles/partial", reason: INTERRUPTED },
    Exemption { name: "redactions-chunks-owed", reason: INTERRUPTED },
    Exemption { name: "franchise", reason: INTERRUPTED },
    Exemption { name: "{bay}inventory/incomplete-load", reason: INTERRUPTED },
    Exemption { name: "{bay}operation", reason: INTERRUPTED },
    Exemption { name: "origin-move", reason: INTERRUPTED },
    Exemption { name: "taint/", reason: SYNC_FAILED },
    Exemption { name: "taint/<name>", reason: SYNC_FAILED },
    Exemption { name: "store.lock", reason: HELD_LOCK },
    Exemption { name: "serve.lock", reason: HELD_LOCK },
    Exemption { name: "{bay}lock", reason: HELD_LOCK },
];

/// Census names no scenario of this build creates yet, so neither bound nor exempted: a run
/// reports each one missing and fails. They need the sparse-clone, redaction, withhold and host
/// scenarios, or a command interrupted or holding its lock; this list only lets the unit test hold the gap exact, so a name
/// that joins or leaves it is noticed. It is not an exemption and the run never reads it.
pub const NOT_YET_CREATED: &[&str] = &[
    "config/fetch-scope",
    "fetched-heads",
    "redactions-applied",
    "served",
    "tombstones",
    "unavailable",
    "unvouched-heads",
    "withheld",
    "{bay}scope",
];

/// What the damage completeness check finds missing: census names no binding or exemption covers,
/// bindings and exemptions the census no longer lists, bindings naming a scenario that does not
/// exist, and bindings whose scenario was built (`created` holds what each built scenario created)
/// without creating the name. A scenario that was not built proves nothing either way; its build
/// failure is the census walk's to report.
pub fn check(census: &Census, bindings: &[Binding], exemptions: &[Exemption], scenarios: &[Scenario], created: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let mut missing: Vec<String> = damage::uncovered(census, bindings, exemptions).into_iter().map(|name| name.path).collect();
    missing.extend(damage::stale(census, bindings, exemptions).into_iter().map(|name| format!("stale binding: {}", name)));
    for binding in bindings {
        if !scenarios.iter().any(|scenario| scenario.id == binding.scenario) {
            missing.push(format!("{}: its scenario {} does not exist", binding.name, binding.scenario));
        } else if created.get(binding.scenario).is_some_and(|names| !names.iter().any(|name| name == binding.name)) {
            missing.push(format!("{}: its scenario {} does not create it", binding.name, binding.scenario));
        }
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const SAMPLE: &str = "<!-- census:start -->\n| Path | Class | Names |\n|---|---|---|\n\
        | `warehouse` | rewritable | `a::B` |\n\
        | `objects/<xx>/<hash>` | history-borne | `a::E` |\n\
        | `mirror-roots` | rewritable | `a::M` |\n\
        <!-- census:end -->\n";

    /// A binding counts only when its scenario exists and, once built, created its name.
    ///
    /// Revert: drop the creation check → a binding on a path its scenario never makes passes, and
    /// its damage would fail on an absent target at run time instead of here.
    #[test]
    fn a_binding_counts_only_when_its_scenario_creates_the_name() {
        let census = Census::parse(SAMPLE, PathBuf::from("sample")).unwrap().unwrap();
        let scenarios = vec![Scenario { id: "history".into(), history: vec![], then: vec![], damage: None }];
        let bindings = [bound("warehouse", FILE), bound("objects/<xx>/<hash>", FILE)];
        let exemptions = [Exemption { name: "mirror-roots", reason: NO_MIRROR_ORACLE }];

        let all = BTreeMap::from([("history".to_string(), vec!["warehouse".to_string(), "objects/<xx>/<hash>".to_string()])]);
        assert_eq!(check(&census, &bindings, &exemptions, &scenarios, &all), Vec::<String>::new());

        let some = BTreeMap::from([("history".to_string(), vec!["warehouse".to_string()])]);
        assert_eq!(check(&census, &bindings, &exemptions, &scenarios, &some), vec!["objects/<xx>/<hash>: its scenario history does not create it"]);

        let elsewhere = [Binding { name: "warehouse", scenario: "nowhere", kinds: FILE }, bound("objects/<xx>/<hash>", FILE)];
        assert_eq!(check(&census, &elsewhere, &exemptions, &scenarios, &BTreeMap::new()), vec!["warehouse: its scenario nowhere does not exist"]);

        assert_eq!(check(&census, &bindings, &[], &scenarios, &all), vec!["mirror-roots"]);
    }

    /// The bindings file covers this tree's census except for names no scenario of this build
    /// creates, and lists nothing the census does not: a new census row fails here until it is
    /// bound or exempted, or, while no scenario creates it, added to `NOT_YET_CREATED`.
    ///
    /// Revert: drop a binding → its name joins the uncovered list.
    #[test]
    fn this_trees_census_is_covered_but_for_what_no_scenario_creates() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let census = Census::load(&source).unwrap().unwrap();
        let mut uncovered: Vec<String> = damage::uncovered(&census, BINDINGS, EXEMPTIONS).into_iter().map(|name| name.path).collect();
        uncovered.sort();
        let mut expected: Vec<&str> = NOT_YET_CREATED.to_vec();
        expected.sort();
        assert_eq!(uncovered, expected);
        assert_eq!(damage::stale(&census, BINDINGS, EXEMPTIONS), Vec::<&str>::new());
    }
}
