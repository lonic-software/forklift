//! The damage bindings: for every census name, the scenario that creates the path and the damage
//! kinds applied to it, or an exemption with its reason. A run checks each binding's scenario
//! creates its name, and fails on a census name neither list covers.
//!
//! Every binding here names a path a run of its scenario was seen to create; a name no scenario of
//! this build creates is left uncovered, so the run says so, rather than bound to a scenario that
//! would only fail its own check.

use std::collections::BTreeMap;

use crate::census::Census;
use crate::damage::{self, Binding, Exemption, Kind, Warehouse};
use crate::scenario::Scenario;

const FILE: &[Kind] = &[Kind::FlipByte, Kind::Truncate, Kind::Zero, Kind::Delete];
/// A lock file left in place between commands: empty, so there is no byte to flip and nothing to cut.
const LOCK_FILE: &[Kind] = &[Kind::Delete];
const FOLDER: &[Kind] = &[Kind::Delete];

const fn bound(name: &'static str, kinds: &'static [Kind]) -> Binding {
    bound_in(name, "history", kinds)
}

const fn bound_in(name: &'static str, scenario: &'static str, kinds: &'static [Kind]) -> Binding {
    Binding { name, scenario, warehouse: Warehouse::Client, kinds }
}

/// A binding damaging the host warehouse the scenario serves.
const fn hosted(name: &'static str, scenario: &'static str, kinds: &'static [Kind]) -> Binding {
    Binding { name, scenario, warehouse: Warehouse::Host, kinds }
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
    bound_in("config/fetch-scope", "sparse", FILE),
    bound_in("fetched-heads", "sparse", FILE),
    bound_in("{bay}scope", "sparse", FILE),
    hosted("served", "withheld", FILE),
    hosted("tombstones", "withheld", FILE),
    hosted("withheld", "withheld", FILE),
    bound_in("unavailable", "withheld", FILE),
    bound_in("redactions-applied", "redacted", FILE),
    bound_in("unvouched-heads", "unvouched", FILE),
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

/// What the damage completeness check finds missing: census names no binding or exemption covers,
/// bindings and exemptions the census no longer lists, bindings naming a scenario that does not
/// exist, and bindings whose scenario was built (`created` holds what each built scenario created in
/// each warehouse, the host's empty when it hosts none) without creating the name in the warehouse
/// the binding damages. A scenario that was not built proves nothing either way; its build failure
/// is the census walk's to report.
pub fn check(census: &Census, bindings: &[Binding], exemptions: &[Exemption], scenarios: &[Scenario], created: &BTreeMap<(String, Warehouse), Vec<String>>) -> Vec<String> {
    let mut missing: Vec<String> = damage::uncovered(census, bindings, exemptions).into_iter().map(|name| name.path).collect();
    missing.extend(damage::stale(census, bindings, exemptions).into_iter().map(|name| format!("stale binding: {}", name)));
    for binding in bindings {
        if !scenarios.iter().any(|scenario| scenario.id == binding.scenario) {
            missing.push(format!("{}: its scenario {} does not exist", binding.name, binding.scenario));
        } else if created.get(&(binding.scenario.to_string(), binding.warehouse)).is_some_and(|names| !names.iter().any(|name| name == binding.name)) {
            let warehouse = match binding.warehouse { Warehouse::Client => "", Warehouse::Host => " on its host" };
            missing.push(format!("{}: its scenario {} does not create it{}", binding.name, binding.scenario, warehouse));
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

        let client = |names: &[&str]| (("history".to_string(), Warehouse::Client), names.iter().map(|name| name.to_string()).collect::<Vec<_>>());
        let host = |names: &[&str]| (("history".to_string(), Warehouse::Host), names.iter().map(|name| name.to_string()).collect::<Vec<_>>());
        let all = BTreeMap::from([client(&["warehouse", "objects/<xx>/<hash>"]), host(&[])]);
        assert_eq!(check(&census, &bindings, &exemptions, &scenarios, &all), Vec::<String>::new());

        let some = BTreeMap::from([client(&["warehouse"]), host(&[])]);
        assert_eq!(check(&census, &bindings, &exemptions, &scenarios, &some), vec!["objects/<xx>/<hash>: its scenario history does not create it"]);

        let elsewhere = [Binding { name: "warehouse", scenario: "nowhere", warehouse: Warehouse::Client, kinds: FILE }, bound("objects/<xx>/<hash>", FILE)];
        assert_eq!(check(&census, &elsewhere, &exemptions, &scenarios, &BTreeMap::new()), vec!["warehouse: its scenario nowhere does not exist"]);

        assert_eq!(check(&census, &bindings, &[], &scenarios, &all), vec!["mirror-roots"]);

        // A name the client creates does not count for a binding damaging the host, nor the reverse.
        let on_host = [hosted("warehouse", "history", FILE), bound("objects/<xx>/<hash>", FILE)];
        assert_eq!(check(&census, &on_host, &exemptions, &scenarios, &all), vec!["warehouse: its scenario history does not create it on its host"]);
        let hosted_only = BTreeMap::from([client(&["objects/<xx>/<hash>"]), host(&["warehouse"])]);
        assert_eq!(check(&census, &on_host, &exemptions, &scenarios, &hosted_only), Vec::<String>::new());
        assert_eq!(check(&census, &bindings, &exemptions, &scenarios, &hosted_only), vec!["warehouse: its scenario history does not create it"]);
    }

    /// The bindings file covers this tree's whole census and lists nothing the census does not: a
    /// new census row fails here until it is bound or exempted.
    ///
    /// Revert: drop a binding → its name joins the uncovered list.
    #[test]
    fn this_trees_census_is_covered() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let census = Census::load(&source).unwrap().unwrap();
        let uncovered: Vec<String> = damage::uncovered(&census, BINDINGS, EXEMPTIONS).into_iter().map(|name| name.path).collect();
        assert_eq!(uncovered, Vec::<String>::new());
        assert_eq!(damage::stale(&census, BINDINGS, EXEMPTIONS), Vec::<&str>::new());
    }
}
