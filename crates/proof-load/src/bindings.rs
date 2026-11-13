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
use crate::routes::{Exemption as RouteExemption, Mount};
use crate::scenario::Scenario;
use crate::writeops::Exemption as WriteExemption;
use crate::writeops::Exemption::{ReadOnly, Unrunnable};

const FILE: &[Kind] = &[Kind::FlipByte, Kind::Truncate, Kind::Zero, Kind::Delete];
/// A lock file left in place between commands: empty, so there is no byte to flip and nothing to cut.
const LOCK_FILE: &[Kind] = &[Kind::Delete];
const FOLDER: &[Kind] = &[Kind::Delete];
/// A lock file that exists only while a command holds it, planted as a holder killed hard leaves it.
const DEAD_HOLDER: &[Kind] = &[Kind::DeadPidLock];

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
    bound("{bay}lock", DEAD_HOLDER),
    bound_in("store.lock", "compacted", DEAD_HOLDER),
    bound_in("serve.lock", "compacted", DEAD_HOLDER),
];

const NO_MIRROR_ORACLE: &str = "no mirror oracle: the corpus configures no mirror url, so nothing reads a mirror back";
const INTERRUPTED: &str = "present only while a command runs or after one is interrupted, and removed when it finishes: no finished history holds one to damage, so the kill-point faults grade it";
const SYNC_FAILED: &str = "written only when a directory sync after a rename fails, and cleared by heal: no finished history holds one to damage, so the power-loss and kill-point faults grade it";
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
];

const READS: &str = "reads the warehouse and writes nothing to it";
const LISTS: &str = "a group's bare form or a listing: reads the warehouse and writes nothing to it";
const GLOBAL: &str = "writes only the operator's own configuration and keys, outside every warehouse";
const PRINTS: &str = "prints what the binary is and touches no warehouse";
const REPLACES_BINARY: &str = "replaces the binary under test, or the shell's aliases for it, outside every warehouse";
const NEEDS_OLDER_FORMAT: &str = "upgrades a warehouse of an older format, and the version under test writes only its own";
const SERVES: &str = "serves until stopped; its writes are the hosted scenarios' requests, which the server kill points grade";

/// The commands the write-op check does not require a traced write of, by schema id (a server
/// leaf as `forklift-server <words>`). A read-only one must still be run by some scenario, and
/// every run of it is checked to change nothing under the tree it ran in.
pub const WRITE_EXEMPTIONS: &[(&str, WriteExemption)] = &[
    ("audit", ReadOnly(READS)), ("diff", ReadOnly(READS)), ("history", ReadOnly(READS)),
    ("query", ReadOnly(READS)), ("show", ReadOnly(READS)), ("stocktake", ReadOnly(READS)),
    ("bay", ReadOnly(LISTS)), ("config", ReadOnly(LISTS)), ("config-list", ReadOnly(LISTS)), ("config-get", ReadOnly(READS)),
    ("conflicts", ReadOnly(LISTS)), ("haul", ReadOnly(LISTS)), ("haul-list", ReadOnly(LISTS)), ("haul-show", ReadOnly(READS)),
    ("manifest", ReadOnly(LISTS)), ("manifest-show", ReadOnly(READS)), ("manifest-get", ReadOnly(READS)), ("manifest-keys", ReadOnly(READS)),
    ("manifest-log", ReadOnly(READS)), ("office", ReadOnly(LISTS)), ("office-list", ReadOnly(LISTS)),
    ("pallet", ReadOnly(LISTS)), ("pallet-list", ReadOnly(LISTS)), ("park-list", ReadOnly(LISTS)),
    ("scope", ReadOnly(LISTS)), ("scope-status", ReadOnly(READS)), ("remote", ReadOnly(READS)),
    ("store", ReadOnly(LISTS)), ("store-status", ReadOnly(READS)), ("store-peek", ReadOnly(READS)),
    ("tag", ReadOnly(LISTS)), ("tag-show", ReadOnly(READS)), ("tag-list", ReadOnly(LISTS)),
    ("redact", ReadOnly(LISTS)), ("redact-list", ReadOnly(LISTS)),
    ("self-alias", ReadOnly(LISTS)), ("self-alias-status", ReadOnly(READS)),
    ("profile", ReadOnly(GLOBAL)), ("profile-list", ReadOnly(GLOBAL)), ("profile-create", ReadOnly(GLOBAL)),
    ("profile-keygen", ReadOnly(GLOBAL)),
    ("forklift-server tombstone list", ReadOnly(READS)), ("forklift-server withhold list", ReadOnly(READS)),
    ("version", Unrunnable(PRINTS)), ("self-update", Unrunnable(REPLACES_BINARY)),
    ("self-alias-install", Unrunnable(REPLACES_BINARY)), ("self-alias-uninstall", Unrunnable(REPLACES_BINARY)),
    ("upgrade", Unrunnable(NEEDS_OLDER_FORMAT)), ("forklift-server upgrade", Unrunnable(NEEDS_OLDER_FORMAT)),
    ("forklift-server serve", Unrunnable(SERVES)),
];

/// The server routes the route check does not require a successful answer of, by the route
/// table's method, path and mount: an exemption at the wrong mount covers nothing and is reported
/// stale.
pub const ROUTE_EXEMPTIONS: &[RouteExemption] = &[
    RouteExemption { method: "GET", path: "/healthz", mount: Mount::Root, reason: "no client command sends it: supervisors and load balancers probe it" },
    RouteExemption { method: "PUT", path: "/warehouses/{warehouse}", mount: Mount::Root, reason: "no client command sends it: an operator creates a warehouse over HTTP, and no scenario serves more than one" },
    // FORK-473: delete the route or give it a caller.
    RouteExemption { method: "GET", path: "/parcels/{parcel}/subtree/{*path}", mount: Mount::Protocol, reason: "no client command sends it: the client's method for it has no caller outside its tests" },
    // FORK-474: the scenario steps that reach these.
    RouteExemption { method: "POST", path: "/objects/missing", mount: Mount::Protocol, reason: "sent by an origin move over withheld recipes and by heal's refetch into a hosted clone, and no scenario reaches either yet" },
    RouteExemption { method: "PUT", path: "/withheld/{hash}/recipe", mount: Mount::Protocol, reason: "sent when a lift's new history reaches content the host withholds, and no scenario builds that yet" },
];

/// What the damage completeness check finds missing: census names no binding or exemption covers,
/// bindings and exemptions the census no longer lists, bindings naming a scenario that does not
/// exist, and bindings whose scenario was built (`created` holds what each built scenario created in
/// each warehouse, the host's empty when it hosts none) without creating the name in the warehouse
/// the binding damages. A scenario that was not built proves nothing either way; its build failure
/// is the census walk's to report. A binding whose kinds all plant their path is not checked against
/// what the scenario created: the path exists only while a command holds it, and the fault makes it.
pub fn check(census: &Census, bindings: &[Binding], exemptions: &[Exemption], scenarios: &[Scenario], created: &BTreeMap<(String, Warehouse), Vec<String>>) -> Vec<String> {
    let mut missing: Vec<String> = damage::uncovered(census, bindings, exemptions).into_iter().map(|name| name.path).collect();
    missing.extend(damage::stale(census, bindings, exemptions).into_iter().map(|name| format!("stale binding: {}", name)));
    for binding in bindings {
        if !scenarios.iter().any(|scenario| scenario.id == binding.scenario) {
            missing.push(format!("{}: its scenario {} does not exist", binding.name, binding.scenario));
        } else if binding.kinds.iter().all(|kind| kind.plants()) {
            continue;
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
        let scenarios = vec![Scenario { id: "history".into(), history: vec![], then: vec![], damage: None, files: crate::scenario::Files::Seeded }];
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

    /// A planted binding names a path its scenario never leaves behind, so the creation check skips
    /// it; a binding that also damages is still checked.
    ///
    /// Revert: drop the planting skip → `{bay}lock` reads as not created by `history`.
    #[test]
    fn a_planted_binding_is_not_checked_against_what_was_built() {
        let doc = "<!-- census:start -->\n| Path | Class | Names |\n|---|---|---|\n| `{bay}lock` | lock | `a::L` |\n<!-- census:end -->\n";
        let census = Census::parse(doc, PathBuf::from("sample")).unwrap().unwrap();
        let scenarios = vec![Scenario { id: "history".into(), history: vec![], then: vec![], damage: None, files: crate::scenario::Files::Seeded }];
        let built = BTreeMap::from([(("history".to_string(), Warehouse::Client), vec!["warehouse".to_string()])]);
        assert_eq!(check(&census, &[bound("{bay}lock", DEAD_HOLDER)], &[], &scenarios, &built), Vec::<String>::new());
        const MIXED: &[Kind] = &[Kind::DeadPidLock, Kind::Delete];
        assert_eq!(check(&census, &[bound("{bay}lock", MIXED)], &[], &scenarios, &built), vec!["{bay}lock: its scenario history does not create it"]);
    }

    /// Every planting binding names a literal path once `{bay}` is the main bay's: a planted path is
    /// absent, so there is no walk to resolve a pattern against.
    ///
    /// Revert: plant `objects/<xx>/<hash>` → it names no file.
    #[test]
    fn planting_bindings_name_literal_paths() {
        for binding in BINDINGS.iter().filter(|binding| binding.kinds.iter().any(|kind| kind.plants())) {
            assert!(!damage::planted(binding.name).contains(['<', '{']), "{} plants a pattern", binding.name);
        }
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

    /// This tree's route table parses, and every route exemption names a route it lists: an
    /// exemption left behind by a removed route fails here, before any run.
    ///
    /// Revert: exempt a route the table does not list → it is reported stale.
    #[test]
    fn this_trees_route_exemptions_name_listed_routes() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let table = crate::routes::RouteTable::load(&source).unwrap().unwrap();
        for exemption in ROUTE_EXEMPTIONS {
            assert!(table.routes.iter().any(|route| exemption.names(route)), "{} {} is not a listed route", exemption.method, exemption.path);
        }
    }
}
