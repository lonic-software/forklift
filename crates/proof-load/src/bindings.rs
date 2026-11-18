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
use crate::routewrites::{Bound, Entry, RouteWrites};
use crate::trace::Kind as Change;
use crate::scenario::{Scenario, Tool};
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
    bound("warehouse-copy", FILE),
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
    bound("{bay}position-copy", FILE),
    bound_in("trust", "signed", FILE),
    bound_in("meta/", "signed", FOLDER),
    bound_in("meta/<name>", "signed", FILE),
    bound_in("objects/<xx>/<hash>.sig", "signed", FILE),
    bound_in("objects/pack/", "packed", FOLDER),
    bound_in("objects/pack/<name>.pack", "packed", FILE),
    bound_in("objects/pack/<name>.idx", "packed", FILE),
    bound_in("objects/pack/<name>.sigs", "packed", FILE),
    bound_in("parked", "parked", FILE),
    bound_in("parked.lock", "parked", LOCK_FILE),
    bound_in("{bay}consolidation", "conflicted", FILE),
    bound_in("{bay}consolidation-copy", "conflicted", FILE),
    bound_in("{bay}consolidation-skeleton", "conflicted", FILE),
    bound_in("{bay}consolidation-skeleton-copy", "conflicted", FILE),
    bound_in("{bay}cherry-pick", "picking", FILE),
    bound_in("{bay}cherry-pick-copy", "picking", FILE),
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
    bound_in("{bay}scope-copy", "sparse", FILE),
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
const KEPT_ONLY: &str = "written only by heal --remove-ref, the removed ref's bytes kept for the operator, and never read back: no outcome depends on it";
const SET_ASIDE: &str = "written only by heal --config, a damaged configuration file's bytes kept for the operator, and read back only to refuse writing over a different copy: no outcome depends on its content";
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
    Exemption { name: "removed-refs/", reason: KEPT_ONLY },
    Exemption { name: "removed-refs/<name>", reason: KEPT_ONLY },
    Exemption { name: "removed-refs/<name>.<n>", reason: KEPT_ONLY },
    Exemption { name: "config/warehouse.toml.damaged", reason: SET_ASIDE },
];

const READS: &str = "reads the warehouse and writes nothing to it";
const LISTS: &str = "a group's bare form or a listing: reads the warehouse and writes nothing to it";
const GLOBAL: &str = "writes only the operator's own configuration and keys, outside every warehouse";
const PRINTS: &str = "prints what the binary is and touches no warehouse";
const REPLACES_BINARY: &str = "replaces the binary under test, or the shell's aliases for it, outside every warehouse";
const NEEDS_OLDER_FORMAT: &str = "upgrades a warehouse of an older format, and the version under test writes only its own";

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

const fn entry(kind: Change, path: &'static str) -> Entry {
    Entry { kind, path }
}

const fn route(method: &'static str, path: &'static str, mount: Mount, bound: Bound) -> RouteWrites {
    RouteWrites { method, path, mount, bound }
}

// Each set is what a CI trace of the version under test showed its route write, checked against
// the handler's source. A write is made atomically: a `<name>.tmp~<pid>-<n>` written, then renamed
// onto the name, so both names are a rename's operands.

// store_object_bytes: the object zstd-compressed at objects/<first two>/<rest>, its fan-out folder
// made first.
const PUT_OBJECT: &[Entry] = &[
    entry(Change::Mkdir, ".forklift/objects/{hash[..2]}"), entry(Change::Write, ".forklift/objects/{hash[..2]}/{hash[2..]}.tmp~*"),
    entry(Change::Rename, ".forklift/objects/{hash[..2]}/{hash[2..]}.tmp~*"), entry(Change::Rename, ".forklift/objects/{hash[..2]}/{hash[2..]}"),
];
// withholding::record_digest: the withheld record rewritten whole. No scenario reaches it yet
// (FORK-474), so this set is from the source alone. The redaction deleter records a digest in the
// same file in the background, and a write a shared set holds credits no route, so this route is
// credited only once it is seen making a write of its own: the step FORK-474 adds fails this
// check until then, by design, rather than letting a background write prove it.
const PUT_WITHHELD_RECIPE: &[Entry] = &[
    entry(Change::Write, ".forklift/withheld.tmp~*"), entry(Change::Rename, ".forklift/withheld.tmp~*"), entry(Change::Rename, ".forklift/withheld"),
];
// store_raw_parcel_signature: a sidecar beside its parcel, objects/<first two>/<rest>.sig.
const PUT_SIGNATURE: &[Entry] = &[
    entry(Change::Write, ".forklift/objects/{hash[..2]}/{hash[2..]}.sig.tmp~*"),
    entry(Change::Rename, ".forklift/objects/{hash[..2]}/{hash[2..]}.sig.tmp~*"), entry(Change::Rename, ".forklift/objects/{hash[..2]}/{hash[2..]}.sig"),
];
// The office's trust file.
const PUT_TRUST: &[Entry] = &[entry(Change::Write, ".forklift/trust.tmp~*"), entry(Change::Rename, ".forklift/trust.tmp~*"), entry(Change::Rename, ".forklift/trust")];
// A ref update: the head at pallets/<name>, or meta/<name> for an `@name`, its parent folders made
// for a nested name, and the commit-graph shards of the parcels it brings, graph/<first two>.
// `{name[1..]}` drops the `@`; for a user pallet it names a meta head with its first letter cut,
// which no update writes. Recreating a deleted pallet clears its tombstone (`clear_tombstone`).
// The parent folders and the tombstone are from the source alone: no scenario nests a name or
// lifts a deleted pallet again.
const UPDATE_PALLET: &[Entry] = &[
    entry(Change::Mkdir, ".forklift/pallets/{name:parents}"), entry(Change::Mkdir, ".forklift/meta/{name:parents[1..]}"),
    entry(Change::Write, ".forklift/tombstones.tmp~*"), entry(Change::Rename, ".forklift/tombstones.tmp~*"), entry(Change::Rename, ".forklift/tombstones"),
    entry(Change::Write, ".forklift/pallets/{name}.tmp~*"), entry(Change::Rename, ".forklift/pallets/{name}.tmp~*"), entry(Change::Rename, ".forklift/pallets/{name}"),
    entry(Change::Mkdir, ".forklift/meta"),
    entry(Change::Write, ".forklift/meta/{name[1..]}.tmp~*"), entry(Change::Rename, ".forklift/meta/{name[1..]}.tmp~*"), entry(Change::Rename, ".forklift/meta/{name[1..]}"),
    entry(Change::Mkdir, ".forklift/graph"),
    entry(Change::Write, ".forklift/graph/*.tmp~*"), entry(Change::Rename, ".forklift/graph/*.tmp~*"), entry(Change::Rename, ".forklift/graph/*"),
];
// A ref deletion: the head removed, each parent folder it leaves empty removed (`unlink_ref`, from
// the source alone), and its last head kept in the tombstones file. A meta pallet is refused.
const DELETE_PALLET: &[Entry] = &[
    entry(Change::Remove, ".forklift/pallets/{name}"), entry(Change::Remove, ".forklift/pallets/{name:parents}"),
    entry(Change::Write, ".forklift/tombstones.tmp~*"), entry(Change::Rename, ".forklift/tombstones.tmp~*"), entry(Change::Rename, ".forklift/tombstones"),
];
// create_warehouse: a `.creating-<id>-<pid>-<n>` staging folder prepared in full, an empty folder
// at the id removed, the staging folder renamed onto it.
const PUT_WAREHOUSE: &[Entry] = &[
    entry(Change::Mkdir, ".creating-{warehouse}-*"), entry(Change::Mkdir, ".creating-{warehouse}-*/**"),
    entry(Change::Write, ".creating-{warehouse}-*/**"), entry(Change::Rename, ".creating-{warehouse}-*/**"),
    entry(Change::Remove, "{warehouse}"), entry(Change::Rename, ".creating-{warehouse}-*"), entry(Change::Rename, "{warehouse}"),
];

/// Every server route the route table lists, bound to what it may write in the warehouse it
/// serves, as a template relative to the host root filled from the request's captures.
pub const ROUTE_WRITES: &[RouteWrites] = &[
    route("GET", "/warehouse", Mount::Protocol, Bound::ReadOnly),
    route("POST", "/objects/missing", Mount::Protocol, Bound::ReadOnly),
    route("POST", "/objects/upload-targets", Mount::Protocol, Bound::ReadOnly),
    route("POST", "/objects/batch", Mount::Protocol, Bound::ReadOnly),
    route("GET", "/parcels/{parcel}/subtree/{*path}", Mount::Protocol, Bound::ReadOnly),
    route("GET", "/objects/{hash}", Mount::Protocol, Bound::ReadOnly),
    route("PUT", "/objects/{hash}", Mount::Protocol, Bound::Writes(PUT_OBJECT)),
    route("PUT", "/withheld/{hash}/recipe", Mount::Protocol, Bound::Writes(PUT_WITHHELD_RECIPE)),
    route("GET", "/signatures/{hash}", Mount::Protocol, Bound::ReadOnly),
    route("PUT", "/signatures/{hash}", Mount::Protocol, Bound::Writes(PUT_SIGNATURE)),
    route("PUT", "/trust", Mount::Protocol, Bound::Writes(PUT_TRUST)),
    route("POST", "/pallets/{*name}", Mount::Protocol, Bound::Writes(UPDATE_PALLET)),
    route("DELETE", "/pallets/{*name}", Mount::Protocol, Bound::Writes(DELETE_PALLET)),
    route("POST", "/holds/{*name}", Mount::Protocol, Bound::ReadOnly),
    route("POST", "/resolve", Mount::Protocol, Bound::ReadOnly),
    route("GET", "/bundles/latest", Mount::Protocol, Bound::ReadOnly),
    route("PUT", "/warehouses/{warehouse}", Mount::Root, Bound::Writes(PUT_WAREHOUSE)),
    route("GET", "/healthz", Mount::Root, Bound::ReadOnly),
];

/// What the server writes on its own, after it answered: work a request or a command started
/// that goes on running in a spawned task. Its redaction round deletes what a redaction or a
/// withhold took away once the warehouse drains (`delete_drained`, through the redaction deleter):
/// loose copies removed (seen in a trace), and from the source alone, packs repacked without them,
/// the chunks-owed journal written or removed, and a withheld recipe's digest recorded.
pub const BACKGROUND_WRITES: &[Entry] = &[
    entry(Change::Remove, ".forklift/objects/*/*"),
    entry(Change::Write, ".forklift/objects/pack/*"), entry(Change::Rename, ".forklift/objects/pack/*"), entry(Change::Remove, ".forklift/objects/pack/*"),
    entry(Change::Write, ".forklift/redactions-chunks-owed.tmp~*"), entry(Change::Rename, ".forklift/redactions-chunks-owed.tmp~*"),
    entry(Change::Rename, ".forklift/redactions-chunks-owed"), entry(Change::Remove, ".forklift/redactions-chunks-owed"),
    entry(Change::Write, ".forklift/withheld.tmp~*"), entry(Change::Rename, ".forklift/withheld.tmp~*"), entry(Change::Rename, ".forklift/withheld"),
];

/// What the server writes admitting a request: nothing, as admission asks a hook over the network
/// and writes no file. Kept as its own set so a version that records admissions must declare it.
pub const ADMISSION_WRITES: &[Entry] = &[];

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
            missing.push(format!("{}: its scenario {} does not create it{}", binding.name, binding.scenario, on_its(binding.warehouse)));
        }
    }
    missing
}

/// Git's damage bindings, over the git layout's rows ([`crate::git_layout::ROWS`]): the same kinds
/// per role as Forklift's (every kind on a file, removal on a folder, a dead holder planted for a
/// lock), on the scenarios git can run. A lock is planted at a name its row matches. The host is
/// a bare repository, laid out as a repository's `.git` is, so its paths are the same rows.
pub const GIT_BINDINGS: &[Binding] = &[
    bound("HEAD", FILE),
    bound_in("ORIG_HEAD", "branched", FILE),
    bound("config", FILE),
    bound("index", FILE),
    bound("logs/**/", FOLDER),
    bound("logs/**", FILE),
    bound("refs/", FOLDER),
    bound("refs/heads/**/", FOLDER),
    bound("refs/heads/**", FILE),
    bound("refs/tags/**/", FOLDER),
    bound_in("refs/remotes/**/", "lifted", FOLDER),
    bound_in("refs/remotes/**", "lifted", FILE),
    bound_in("refs/stash", "parked", FILE),
    bound("objects/", FOLDER),
    bound("objects/info/", FOLDER),
    bound("objects/pack/", FOLDER),
    bound("objects/<xx>/", FOLDER),
    bound("objects/<xx>/<hash>", FILE),
    bound("index.lock", DEAD_HOLDER),
    bound("HEAD.lock", DEAD_HOLDER),
    bound("refs/heads/main.lock", DEAD_HOLDER),
    hosted("HEAD", "lifted", FILE),
    hosted("config", "lifted", FILE),
    hosted("refs/heads/**", "lifted", FILE),
    hosted("objects/<xx>/", "lifted", FOLDER),
    hosted("objects/<xx>/<hash>", "lifted", FILE),
];

/// Git tuned's bindings beyond [`GIT_BINDINGS`]: the folders of the fsmonitor daemon, which only a
/// tuned work tree runs, where this git has one.
pub const GIT_TUNED_BINDINGS: &[Binding] = &[
    bound("fsmonitor--daemon/", FOLDER),
    bound("fsmonitor--daemon/cookies/", FOLDER),
];

/// A git subject's damage bindings and exemptions: git at its defaults exempts what only the
/// fsmonitor daemon writes, and so does git tuned where `daemon` says this git has none.
pub fn git_subject(tool: Tool, daemon: bool) -> (Vec<Binding>, Vec<Exemption>) {
    let mut bindings = GIT_BINDINGS.to_vec();
    let mut exemptions = GIT_EXEMPTIONS.to_vec();
    let unwatched = match tool {
        Tool::GitTuned if daemon => None,
        Tool::GitTuned => Some("not written: this git has no fsmonitor daemon"),
        _ => Some("not written: only the fsmonitor daemon writes it, and git at its defaults runs none"),
    };
    match unwatched {
        None => bindings.extend_from_slice(GIT_TUNED_BINDINGS),
        Some(reason) => exemptions.extend(GIT_TUNED_BINDINGS.iter().map(|binding| Exemption { name: binding.name, reason })),
    }
    (bindings, exemptions)
}

const GIT_NOT_WRITTEN: &str = "not written: only a gc, a repack or maintenance writes it, and no scenario git runs does either";

/// Git layout rows that need damage and are not damaged, with the reason. One a scenario git
/// builds does create is stale, and fails the check.
pub const GIT_EXEMPTIONS: &[Exemption] = &[
    Exemption { name: "FETCH_HEAD", reason: "not written: no scenario git runs fetches" },
    Exemption { name: "refs/tags/**", reason: "not written: no scenario makes a tag" },
    Exemption { name: "packed-refs", reason: GIT_NOT_WRITTEN },
    Exemption { name: "info/refs", reason: GIT_NOT_WRITTEN },
    Exemption { name: "objects/info/packs", reason: GIT_NOT_WRITTEN },
    Exemption { name: "objects/info/commit-graph", reason: GIT_NOT_WRITTEN },
    Exemption { name: "objects/info/commit-graphs/**", reason: GIT_NOT_WRITTEN },
    Exemption { name: "objects/pack/multi-pack-index", reason: GIT_NOT_WRITTEN },
    Exemption { name: "objects/pack/<name>.pack", reason: GIT_NOT_WRITTEN },
    Exemption { name: "objects/pack/<name>.idx", reason: GIT_NOT_WRITTEN },
    Exemption { name: "objects/pack/<name>.rev", reason: GIT_NOT_WRITTEN },
    Exemption { name: "objects/pack/<name>.bitmap", reason: GIT_NOT_WRITTEN },
];

/// What git's damage bindings miss: every layout row that needs damage and has neither a binding
/// nor an exemption; an exemption that names no row, or a row a git build created; and a binding
/// whose scenario git does not build, or whose built walk of its warehouse holds no path of its
/// row. `walks` holds each walk of each scenario git built, by id and warehouse (a scenario that
/// hosts nothing has no host walk); `runnable` every scenario git can run, so one that failed to
/// build is the walk check's to report, never this one's.
pub fn check_git(bindings: &[Binding], exemptions: &[Exemption], runnable: &[String], walks: &BTreeMap<(String, Warehouse), Vec<String>>) -> Vec<String> {
    use crate::git_layout::{self, ROWS};
    let mut missing = Vec::new();
    let plants = |binding: &Binding| binding.kinds.iter().all(|kind| kind.plants());
    for row in ROWS.iter().filter(|row| row.class.needs_damage()) {
        let bound = bindings.iter().any(|binding| binding.name == row.pattern
            || (plants(binding) && git_layout::row_of(binding.name).is_some_and(|planted| planted.pattern == row.pattern)));
        if !bound && !exemptions.iter().any(|exemption| exemption.name == row.pattern) {
            missing.push(row.pattern.to_string());
        }
    }
    for exemption in exemptions {
        match git_layout::named(exemption.name) {
            None => missing.push(format!("stale exemption: {} is no git layout row", exemption.name)),
            Some(_) => missing.extend(walks.iter().filter(|(_, walked)| !git_layout::reached(exemption.name, walked).is_empty())
                .map(|((scenario, warehouse), _)| format!("stale exemption: {}: {} creates it{}", exemption.name, scenario, on_its(*warehouse)))),
        }
    }
    for binding in bindings {
        if plants(binding) {
            if !git_layout::row_of(binding.name).is_some_and(|row| row.class == crate::census::Class::Lock) {
                missing.push(format!("{}: plants a path no lock row matches", binding.name));
            }
        } else if git_layout::named(binding.name).is_none() {
            missing.push(format!("{}: is no git layout row", binding.name));
        }
        if !runnable.iter().any(|scenario| scenario == binding.scenario) {
            missing.push(format!("{}: its scenario {} is not one git runs", binding.name, binding.scenario));
        } else if !plants(binding) {
            let built = walks.contains_key(&(binding.scenario.to_string(), Warehouse::Client));
            let walked = walks.get(&(binding.scenario.to_string(), binding.warehouse));
            // Built, with no walk of the binding's warehouse: a host binding on a scenario that
            // hosts nothing.
            if (built && walked.is_none()) || walked.is_some_and(|walked| git_layout::reached(binding.name, walked).is_empty()) {
                missing.push(format!("{}: its scenario {} does not create it{}", binding.name, binding.scenario, on_its(binding.warehouse)));
            }
        }
    }
    missing
}

/// A binding over a real history's subject: every one damages the client, which is all a history
/// subject is.
const fn over_history(name: &'static str, kinds: &'static [Kind]) -> Binding {
    bound_in(name, crate::faulted::HISTORY_SCENARIO, kinds)
}

/// Forklift's damage bindings over a real history: each census name an import of a git history
/// holds, with the same kinds per class as [`BINDINGS`], and the bay's lock planted as a dead
/// holder. Checked by [`check_history`] against the walk of the pristine subject.
pub const HISTORY_BINDINGS: &[Binding] = &[
    over_history("warehouse", FILE),
    over_history("warehouse-copy", FILE),
    over_history("format.lock", LOCK_FILE),
    over_history("refs.lock", LOCK_FILE),
    over_history("store-access.lock", LOCK_FILE),
    over_history("config/", FOLDER),
    over_history("config/warehouse.toml", FILE),
    over_history("config/warehouse.toml.lock", LOCK_FILE),
    over_history("objects/", FOLDER),
    over_history("objects/<xx>/", FOLDER),
    over_history("objects/<xx>/<hash>", FILE),
    over_history("objects/pack/", FOLDER),
    over_history("objects/pack/<name>.pack", FILE),
    over_history("objects/pack/<name>.idx", FILE),
    over_history("objects/pack/densify-pending", FILE),
    over_history("pallets/", FOLDER),
    over_history("pallets/<name>", FILE),
    over_history("graph/", FOLDER),
    over_history("graph/<xx>", FILE),
    over_history("meta/", FOLDER),
    over_history("meta/<name>", FILE),
    over_history("parked", FILE),
    over_history("{bay}inventory/", FOLDER),
    over_history("{bay}inventory/metadata", FILE),
    over_history("{bay}inventory/inv_/", FOLDER),
    over_history("{bay}inventory/inv_/<xx>/", FOLDER),
    over_history("{bay}inventory/inv_/<xx>/<hash>/", FOLDER),
    over_history("{bay}inventory/inv_/<xx>/<hash>/data", FILE),
    over_history("{bay}position", FILE),
    over_history("{bay}position-copy", FILE),
    over_history("{bay}lock", DEAD_HOLDER),
];

/// Census names an import holds that are not damaged over a history, with the reason.
pub const HISTORY_EXEMPTIONS: &[Exemption] = &[];

/// What forklift's history bindings miss against `walked`, the walk of the pristine history
/// subject's state folder: each census name needing damage the subject holds that neither a
/// binding nor an exemption names; each binding the subject does not hold, unless it plants its
/// path; each exemption the subject does not hold, which then excuses nothing; each name the
/// census no longer lists; and each binding the role map gives no role. Names the subject does
/// not hold need no entry: the fixture bindings answer for them.
pub fn check_history(census: &Census, bindings: &[Binding], exemptions: &[Exemption], walked: &[String]) -> Vec<String> {
    let held = |name: &str| walked.iter().any(|path| crate::census::matches(name, path));
    let mut missing: Vec<String> = damage::uncovered(census, bindings, exemptions).into_iter()
        .filter(|name| held(&name.path))
        .map(|name| format!("{}: the history subject holds it", name.path)).collect();
    missing.extend(damage::stale(census, bindings, exemptions).into_iter().map(|name| format!("stale binding: {}", name)));
    for binding in bindings {
        if !binding.kinds.iter().all(|kind| kind.plants()) && !held(binding.name) {
            missing.push(format!("{}: the history subject does not hold it", binding.name));
        }
    }
    missing.extend(exemptions.iter().filter(|exemption| !held(exemption.name))
        .map(|exemption| format!("stale exemption: {}: the history subject does not hold it", exemption.name)));
    missing
}

/// Git's damage bindings over a real history, the same for git at its defaults and tuned: the
/// layout rows a mirror given a work tree holds, where [`GIT_BINDINGS`] exempts what only a gc
/// writes, since a mirror holds its refs in `packed-refs` and its objects in a pack. The subject's
/// build writes the probe ref's loose file, and nothing before the damage writes a loose object.
/// Checked by [`check_git`] against the walk of the pristine subject.
pub const HISTORY_GIT_BINDINGS: &[Binding] = &[
    over_history("HEAD", FILE),
    over_history("config", FILE),
    over_history("index", FILE),
    over_history("logs/**/", FOLDER),
    over_history("logs/**", FILE),
    over_history("refs/", FOLDER),
    over_history("refs/heads/**/", FOLDER),
    over_history("refs/heads/**", FILE),
    over_history("refs/tags/**/", FOLDER),
    over_history("packed-refs", FILE),
    over_history("objects/", FOLDER),
    over_history("objects/info/", FOLDER),
    over_history("objects/pack/", FOLDER),
    over_history("objects/pack/<name>.pack", FILE),
    over_history("objects/pack/<name>.idx", FILE),
    over_history("objects/pack/<name>.rev", FILE),
    over_history("index.lock", DEAD_HOLDER),
    over_history("HEAD.lock", DEAD_HOLDER),
];

const HISTORY_NOT_WRITTEN: &str = "not written: a mirror given a work tree holds none, and the probe record writes none";
const HISTORY_PACKED: &str = "not written: a mirror holds every object in a pack, and nothing loose is written before the damage";

/// Git layout rows that need damage and are not damaged over a history, with the reason.
pub const HISTORY_GIT_EXEMPTIONS: &[Exemption] = &[
    Exemption { name: "ORIG_HEAD", reason: HISTORY_NOT_WRITTEN },
    Exemption { name: "FETCH_HEAD", reason: HISTORY_NOT_WRITTEN },
    Exemption { name: "fsmonitor--daemon/", reason: "not written: no fsmonitor daemon runs over a history" },
    Exemption { name: "fsmonitor--daemon/cookies/", reason: "not written: no fsmonitor daemon runs over a history" },
    Exemption { name: "refs/stash", reason: HISTORY_NOT_WRITTEN },
    Exemption { name: "refs/tags/**", reason: "not written: a mirror packs every tag into packed-refs and leaves no loose tag file" },
    Exemption { name: "refs/remotes/**/", reason: HISTORY_NOT_WRITTEN },
    Exemption { name: "refs/remotes/**", reason: HISTORY_NOT_WRITTEN },
    Exemption { name: "info/refs", reason: HISTORY_NOT_WRITTEN },
    Exemption { name: "objects/info/packs", reason: HISTORY_NOT_WRITTEN },
    Exemption { name: "objects/info/commit-graph", reason: HISTORY_NOT_WRITTEN },
    Exemption { name: "objects/info/commit-graphs/**", reason: HISTORY_NOT_WRITTEN },
    Exemption { name: "objects/pack/multi-pack-index", reason: HISTORY_NOT_WRITTEN },
    Exemption { name: "objects/pack/<name>.bitmap", reason: HISTORY_NOT_WRITTEN },
    Exemption { name: "objects/<xx>/", reason: HISTORY_PACKED },
    Exemption { name: "objects/<xx>/<hash>", reason: HISTORY_PACKED },
];

/// A subject's damage bindings and exemptions over a real history.
pub fn history_subject(tool: Tool) -> (&'static [Binding], &'static [Exemption]) {
    match tool {
        Tool::Forklift => (HISTORY_BINDINGS, HISTORY_EXEMPTIONS),
        Tool::Git | Tool::GitTuned => (HISTORY_GIT_BINDINGS, HISTORY_GIT_EXEMPTIONS),
    }
}

/// What `tool`'s history bindings miss against `walked`, the walk of its pristine history
/// subject's state folder: [`check_history`] for forklift, and for git [`check_git`] over that
/// one walk.
pub fn check_history_subject(tool: Tool, census: &Census, walked: &[String]) -> Vec<String> {
    let (bindings, exemptions) = history_subject(tool);
    let mut missing = match tool {
        Tool::Forklift => check_history(census, bindings, exemptions, walked),
        Tool::Git | Tool::GitTuned => {
            let scenario = crate::faulted::HISTORY_SCENARIO.to_string();
            let walks = [((scenario.clone(), Warehouse::Client), walked.to_vec())].into_iter().collect();
            check_git(bindings, exemptions, &[scenario], &walks)
        }
    };
    missing.extend(roleless(tool, bindings));
    missing
}

/// Each of `bindings` the role map gives no role under `tool`, named: such a binding forms no
/// (role, kind) group, so [`history_rows`] would never run it.
fn roleless(tool: Tool, bindings: &[Binding]) -> Vec<String> {
    bindings.iter().filter(|binding| crate::git_layout::role_of(tool, binding.name).is_none())
        .map(|binding| format!("{}: the role map gives it no role", binding.name)).collect()
}

/// The damage rows a history runs for `tool`: for each (role, kind) its bindings reach, one
/// binding picked by `seed`, as (binding, kind, role), in (role, kind) order. A binding the role
/// map gives no role is left out; [`check_history_subject`] names it.
pub fn history_rows(tool: Tool, seed: u64) -> Vec<(Binding, Kind, crate::git_layout::Role)> {
    let mut groups: BTreeMap<(crate::git_layout::Role, Kind), Vec<Binding>> = BTreeMap::new();
    for binding in history_subject(tool).0 {
        let Some(role) = crate::git_layout::role_of(tool, binding.name) else { continue };
        for kind in binding.kinds {
            groups.entry((role, *kind)).or_default().push(binding.clone());
        }
    }
    let mut state = seed;
    groups.into_iter().map(|((role, kind), bindings)| {
        let picked = bindings[(crate::history::splitmix(&mut state) % bindings.len() as u64) as usize].clone();
        (picked, kind, role)
    }).collect()
}

/// How a message names the warehouse a path is in: nothing for the client's.
fn on_its(warehouse: Warehouse) -> &'static str {
    match warehouse { Warehouse::Client => "", Warehouse::Host => " on its host" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The git bindings over git's layout: the shipped set passes over walks that create what it
    /// binds, a damage row with no binding or exemption is named, an exemption over a path a scenario
    /// creates is stale, and a binding over a path its scenario never creates is named.
    ///
    /// Revert: skip the walk check for bindings → the uncreated binding passes; skip the stale
    /// exemption check → the created exempt path passes.
    #[test]
    fn git_bindings_are_checked_against_the_layout_and_the_walks() {
        let (git, git_exemptions) = git_subject(Tool::Git, true);
        let runnable: Vec<String> = ["history", "branched", "parked", "lifted"].map(String::from).to_vec();
        let all: Vec<String> = ["HEAD", "ORIG_HEAD", "config", "index", "logs/", "logs/refs/heads/main", "refs/", "refs/heads/",
            "refs/heads/main", "refs/tags/", "refs/stash", "refs/remotes/", "refs/remotes/origin/", "refs/remotes/origin/main",
            "objects/", "objects/info/", "objects/pack/", "objects/ab/", "objects/ab/cdef"]
            .map(String::from).to_vec();
        let bare: Vec<String> = ["HEAD", "config", "refs/", "refs/heads/", "refs/heads/main", "objects/", "objects/ab/", "objects/ab/cdef"]
            .map(String::from).to_vec();
        let mut walks: BTreeMap<(String, Warehouse), Vec<String>> = runnable.iter().map(|scenario| ((scenario.clone(), Warehouse::Client), all.clone())).collect();
        walks.insert(("lifted".to_string(), Warehouse::Host), bare.clone());
        assert_eq!(check_git(&git, &git_exemptions, &runnable, &walks), Vec::<String>::new());

        // A host binding is checked against the host's walk, and a scenario that hosts nothing
        // creates nothing on a host.
        let mut no_host_object = walks.clone();
        no_host_object.get_mut(&("lifted".to_string(), Warehouse::Host)).unwrap().retain(|path| path != "objects/ab/cdef");
        assert_eq!(check_git(&git, &git_exemptions, &runnable, &no_host_object),
            vec!["objects/<xx>/<hash>: its scenario lifted does not create it on its host".to_string()]);
        let mut unhosted = walks.clone();
        unhosted.remove(&("lifted".to_string(), Warehouse::Host));
        assert_eq!(check_git(&git, &git_exemptions, &runnable, &unhosted).len(), 5);
        let mut host_packed = walks.clone();
        host_packed.get_mut(&("lifted".to_string(), Warehouse::Host)).unwrap().push("packed-refs".to_string());
        assert_eq!(check_git(&git, &git_exemptions, &runnable, &host_packed),
            vec!["stale exemption: packed-refs: lifted creates it on its host".to_string()]);

        let without_index: Vec<Binding> = git.iter().filter(|binding| binding.name != "index").cloned().collect();
        assert_eq!(check_git(&without_index, &git_exemptions, &runnable, &walks), vec!["index".to_string()]);

        let mut packed = walks.clone();
        packed.get_mut(&("history".to_string(), Warehouse::Client)).unwrap().push("packed-refs".to_string());
        assert_eq!(check_git(&git, &git_exemptions, &runnable, &packed), vec!["stale exemption: packed-refs: history creates it".to_string()]);

        let mut no_config = walks.clone();
        no_config.get_mut(&("history".to_string(), Warehouse::Client)).unwrap().retain(|path| path != "config");
        let missing = check_git(&git, &git_exemptions, &runnable, &no_config);
        assert!(missing.iter().any(|line| line.starts_with("config: its scenario history does not create it")), "{:?}", missing);
    }

    /// Git tuned binds the fsmonitor daemon's folders where this git has a daemon, and exempts them
    /// where it has none, as git at its defaults always does; either exemption over a walk that
    /// holds them is stale.
    ///
    /// Revert: give git tuned the defaults' set → its daemon folders are named unbound; bind them
    /// for git at its defaults → its walk, which never holds them, fails both bindings.
    #[test]
    fn git_tuned_binds_the_daemon_folders_only_where_it_runs_one() {
        let runnable: Vec<String> = ["history", "branched", "parked", "lifted"].map(String::from).to_vec();
        let plain: Vec<String> = ["HEAD", "ORIG_HEAD", "config", "index", "logs/", "logs/refs/heads/main", "refs/", "refs/heads/",
            "refs/heads/main", "refs/tags/", "refs/stash", "refs/remotes/", "refs/remotes/origin/", "refs/remotes/origin/main",
            "objects/", "objects/info/", "objects/pack/", "objects/ab/", "objects/ab/cdef"]
            .map(String::from).to_vec();
        let watched: Vec<String> = plain.iter().cloned()
            .chain(["fsmonitor--daemon.ipc", "fsmonitor--daemon/", "fsmonitor--daemon/cookies/"].map(String::from)).collect();
        let bare: Vec<String> = ["HEAD", "config", "refs/", "refs/heads/", "refs/heads/main", "objects/", "objects/ab/", "objects/ab/cdef"]
            .map(String::from).to_vec();
        let walks = |client: &Vec<String>| -> BTreeMap<(String, Warehouse), Vec<String>> {
            let mut walks: BTreeMap<_, _> = runnable.iter().map(|scenario| ((scenario.clone(), Warehouse::Client), client.clone())).collect();
            walks.insert(("lifted".to_string(), Warehouse::Host), bare.clone());
            walks
        };
        let check = |tool: Tool, daemon: bool, client: &Vec<String>| {
            let (bindings, exemptions) = git_subject(tool, daemon);
            check_git(&bindings, &exemptions, &runnable, &walks(client))
        };
        assert_eq!(check(Tool::GitTuned, true, &watched), Vec::<String>::new());
        assert_eq!(check(Tool::GitTuned, false, &plain), Vec::<String>::new());
        assert_eq!(check(Tool::Git, true, &plain), Vec::<String>::new());
        assert_eq!(check(Tool::Git, false, &plain), Vec::<String>::new());
        // A daemon folder in a walk the subject exempts it for is a stale exemption, in each of
        // the four scenarios.
        assert_eq!(check(Tool::Git, true, &watched).len(), 8, "{:?}", check(Tool::Git, true, &watched));
        assert!(check(Tool::GitTuned, false, &watched).iter().all(|line| line.starts_with("stale exemption: fsmonitor--daemon/")));
        // Bound, a folder the walk lacks is named.
        assert_eq!(check(Tool::GitTuned, true, &plain), vec![
            "fsmonitor--daemon/: its scenario history does not create it".to_string(),
            "fsmonitor--daemon/cookies/: its scenario history does not create it".to_string(),
        ]);
    }

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

    /// A history binding the role map gives no role is named, under either subject.
    ///
    /// Revert: name none → both are silent.
    #[test]
    fn a_history_binding_with_no_role_is_named() {
        let unknown = [over_history("no-such-path", FILE)];
        for tool in [Tool::Forklift, Tool::Git] {
            assert_eq!(roleless(tool, &unknown), vec!["no-such-path: the role map gives it no role".to_string()], "{:?}", tool);
            assert_eq!(roleless(tool, history_subject(tool).0), Vec::<String>::new(), "{:?}", tool);
        }
    }

    /// A history runs one row per (role, kind) its bindings reach, per subject: each row's binding
    /// has that role and kind, and which binding stands for a (role, kind) is the seed's pick.
    ///
    /// Revert: pick a group's first binding whatever the seed → no group's pick varies.
    #[test]
    fn a_history_runs_one_row_per_role_and_kind() {
        for tool in [Tool::Forklift, Tool::Git] {
            let groups: std::collections::BTreeSet<(crate::git_layout::Role, Kind)> = history_subject(tool).0.iter()
                .flat_map(|binding| binding.kinds.iter().map(move |kind| (crate::git_layout::role_of(tool, binding.name).unwrap(), *kind))).collect();
            let picks: Vec<Vec<(Binding, Kind, crate::git_layout::Role)>> = (0..20).map(|seed| history_rows(tool, seed)).collect();
            for rows in &picks {
                assert_eq!(rows.iter().map(|(_, kind, role)| (*role, *kind)).collect::<Vec<_>>(), groups.iter().copied().collect::<Vec<_>>(), "{:?}", tool);
                for (binding, kind, role) in rows {
                    assert!(binding.kinds.contains(kind) && crate::git_layout::role_of(tool, binding.name) == Some(*role), "{:?}: {:?}", tool, binding);
                }
            }
            assert!(picks.iter().any(|rows| rows != &picks[0]), "{:?}: the seed picks nothing", tool);
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
