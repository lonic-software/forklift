//! Git's damage universe: the harness-kept layout of a git repository, the counterpart of the
//! census Forklift's format documents, since git has no such document of its own. Each row is a
//! path pattern relative to the repository's `.git`, a class from the census's closed set and a
//! role, and one role map pairs the rows with every census path by role, so a damage of one
//! subject's file can be set beside the same role's damage in the other.
//!
//! A walk after each git build that finds a path no row classifies fails the run by name, and so
//! does a census path the role map has no role for: a new path either tool writes is never left
//! unpaired silently. The table was taken from git 2.55.0, the version the plate names.

use crate::census::{self, Census, Class};

/// What a stored path is for, in terms both subjects share.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// Which line of history the work tree is on.
    HeadRef,
    /// A named line of history.
    BranchRef,
    /// Named lines of history kept together in one file.
    PackedRefs,
    /// One stored object, kept loose.
    Object,
    /// Objects packed together.
    Pack,
    /// The index of a pack, without which the pack cannot be read.
    PackIndex,
    /// What the next record will hold.
    IndexOrStaging,
    /// What each ref pointed at before, kept for recovery.
    Log,
    /// The repository's own settings.
    Config,
    /// Held while a command changes what it guards.
    Lock,
    /// Rebuilt from what else is stored.
    DerivedCache,
    /// Proof of who wrote an object.
    Signature,
    /// What the repository says of itself and of its history beyond refs and objects.
    Metadata,
    /// Work set aside and listed for picking up again.
    ParkedWork,
    /// Nothing in the other subject plays the same part.
    NoCounterpart,
}

/// One row of the git layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct LayoutRow {
    /// A pattern as the census writes one, relative to `.git`: `<…>` stands for one or more
    /// characters, `**` for any number of segments, a trailing `/` for a folder.
    pub pattern: &'static str,
    pub class: Class,
    pub role: Role,
}

const fn row(pattern: &'static str, class: Class, role: Role) -> LayoutRow {
    LayoutRow { pattern, class, role }
}

/// Every path git writes in a repository the harness builds, most specific first: a path's row is
/// the first that matches it, so `objects/pack/` is a pack folder, never a loose object's.
pub const ROWS: &[LayoutRow] = &[
    // A lock is a lock wherever it is; git leaves none behind once a command exits.
    row("**/<name>.lock", Class::Lock, Role::Lock),
    row("HEAD", Class::Rewritable, Role::HeadRef),
    row("ORIG_HEAD", Class::Derived, Role::DerivedCache),
    row("FETCH_HEAD", Class::Derived, Role::DerivedCache),
    // The last commit message as written, which git never reads back.
    row("COMMIT_EDITMSG", Class::Transient, Role::NoCounterpart),
    row("config", Class::Rewritable, Role::Config),
    // Copied from git's template folder at creation; git never reads them, and the exclude list
    // holds only comments.
    row("description", Class::Foreign, Role::NoCounterpart),
    row("hooks/", Class::Foreign, Role::NoCounterpart),
    row("hooks/<name>", Class::Foreign, Role::NoCounterpart),
    row("info/", Class::Foreign, Role::NoCounterpart),
    row("info/exclude", Class::Foreign, Role::NoCounterpart),
    row("info/refs", Class::Derived, Role::DerivedCache),
    row("index", Class::Rewritable, Role::IndexOrStaging),
    row("logs/**/", Class::Rewritable, Role::Log),
    row("logs/**", Class::Rewritable, Role::Log),
    row("refs/", Class::Rewritable, Role::BranchRef),
    row("refs/stash", Class::Rewritable, Role::ParkedWork),
    row("refs/heads/**/", Class::Rewritable, Role::BranchRef),
    row("refs/heads/**", Class::Rewritable, Role::BranchRef),
    row("refs/tags/**/", Class::Rewritable, Role::BranchRef),
    row("refs/tags/**", Class::Rewritable, Role::BranchRef),
    // What a push or a fetch last saw of the remote's branches, rewritten by the next fetch: the
    // part Forklift's record of fetched heads plays.
    row("refs/remotes/**/", Class::Derived, Role::Metadata),
    row("refs/remotes/**", Class::Derived, Role::Metadata),
    row("packed-refs", Class::Rewritable, Role::PackedRefs),
    row("objects/", Class::HistoryBorne, Role::Object),
    row("objects/info/", Class::Derived, Role::DerivedCache),
    row("objects/info/packs", Class::Derived, Role::DerivedCache),
    row("objects/info/commit-graph", Class::Derived, Role::DerivedCache),
    row("objects/info/commit-graphs/**", Class::Derived, Role::DerivedCache),
    row("objects/pack/", Class::HistoryBorne, Role::Pack),
    row("objects/pack/multi-pack-index", Class::Derived, Role::DerivedCache),
    row("objects/pack/<name>.pack", Class::HistoryBorne, Role::Pack),
    row("objects/pack/<name>.idx", Class::HistoryBorne, Role::PackIndex),
    row("objects/pack/<name>.rev", Class::Derived, Role::PackIndex),
    row("objects/pack/<name>.bitmap", Class::Derived, Role::DerivedCache),
    row("objects/<xx>/", Class::HistoryBorne, Role::Object),
    row("objects/<xx>/<hash>", Class::HistoryBorne, Role::Object),
];

/// The row a walked path (relative to `.git`, a folder ending in `/`) belongs to: the first that
/// matches it.
pub fn row_of(path: &str) -> Option<&'static LayoutRow> {
    ROWS.iter().find(|row| census::matches(row.pattern, path))
}

/// The row a pattern of [`ROWS`] names.
pub fn named(pattern: &str) -> Option<&'static LayoutRow> {
    ROWS.iter().find(|row| row.pattern == pattern)
}

/// The paths of a walk no row classifies.
pub fn unclassified(walked: &[String]) -> Vec<&str> {
    walked.iter().map(String::as_str).filter(|path| row_of(path).is_none()).collect()
}

/// The paths of a walk whose row is the one `pattern` names, each folder without its trailing
/// slash: what a damage of that row may hit.
pub fn reached<'a>(pattern: &str, walked: &'a [String]) -> Vec<&'a str> {
    walked.iter().filter(|path| row_of(path).is_some_and(|row| row.pattern == pattern))
        .map(|path| path.trim_end_matches('/')).collect()
}

/// One of the paths of a walk whose row `pattern` names, chosen by the seed.
pub fn resolve(pattern: &str, walked: &[String], seed: u64) -> Option<String> {
    let reached = reached(pattern, walked);
    (!reached.is_empty()).then(|| reached[(seed % reached.len() as u64) as usize].to_string())
}

/// The role of every census path, by the path as the census writes it.
pub const CENSUS_ROLES: &[(&str, Role)] = &[
    ("warehouse", Role::Metadata),
    ("warehouse-copy", Role::Metadata),
    ("format.lock", Role::Lock),
    ("objects/", Role::Object),
    ("objects/<xx>/", Role::Object),
    ("objects/<xx>/<hash>", Role::Object),
    ("objects/<xx>/<hash>.sig", Role::Signature),
    ("objects/pack/", Role::Pack),
    ("objects/pack/<name>.pack", Role::Pack),
    ("objects/pack/<name>.idx", Role::PackIndex),
    ("objects/pack/<name>.sigs", Role::Signature),
    ("objects/pack/densify-pending", Role::DerivedCache),
    ("objects/pack/.compact-<name>.tmp", Role::NoCounterpart),
    ("pallets/", Role::BranchRef),
    ("pallets/<name>", Role::BranchRef),
    ("meta/", Role::Metadata),
    ("meta/<name>", Role::Metadata),
    ("removed-refs/", Role::Log),
    ("removed-refs/<name>", Role::Log),
    ("removed-refs/<name>.<n>", Role::Log),
    ("trust", Role::Signature),
    ("served", Role::Metadata),
    ("tombstones", Role::Metadata),
    ("sealed", Role::Metadata),
    ("redactions-applied", Role::Metadata),
    ("redactions-chunks-owed", Role::Metadata),
    ("withheld", Role::Metadata),
    ("unavailable", Role::Metadata),
    ("unvouched-heads", Role::Metadata),
    ("held-heads", Role::Metadata),
    ("fetched-heads", Role::Metadata),
    ("origin", Role::Config),
    ("origin-move", Role::Metadata),
    ("franchise", Role::Metadata),
    ("parked", Role::ParkedWork),
    ("mirror-roots", Role::Metadata),
    ("mirror.git/**", Role::NoCounterpart),
    (".gitignore", Role::NoCounterpart),
    ("config/", Role::Config),
    ("config/warehouse.toml", Role::Config),
    ("config/warehouse.toml.lock", Role::Lock),
    ("config/fetch-scope", Role::Config),
    ("taint/", Role::Metadata),
    ("taint/<name>", Role::Metadata),
    ("graph/", Role::DerivedCache),
    ("graph/<xx>", Role::DerivedCache),
    ("bundles/", Role::DerivedCache),
    ("bundles/latest", Role::DerivedCache),
    ("bundles/partial", Role::DerivedCache),
    ("store.lock", Role::Lock),
    ("serve.lock", Role::Lock),
    ("store-access.lock", Role::Lock),
    ("refs.lock", Role::Lock),
    ("mirror.lock", Role::Lock),
    ("origin-move.lock", Role::Lock),
    ("parked.lock", Role::Lock),
    ("bays/", Role::NoCounterpart),
    ("bays/<name>/", Role::NoCounterpart),
    ("bays/<name>/path", Role::NoCounterpart),
    ("{bay}inventory/", Role::IndexOrStaging),
    ("{bay}inventory/metadata", Role::IndexOrStaging),
    ("{bay}inventory/inv_/", Role::IndexOrStaging),
    ("{bay}inventory/inv_/<xx>/", Role::IndexOrStaging),
    ("{bay}inventory/inv_/<xx>/<hash>/", Role::IndexOrStaging),
    ("{bay}inventory/inv_/<xx>/<hash>/data", Role::IndexOrStaging),
    ("{bay}inventory/incomplete-load", Role::IndexOrStaging),
    ("{bay}position", Role::HeadRef),
    ("{bay}scope", Role::Config),
    ("{bay}cherry-pick", Role::Metadata),
    ("{bay}consolidation", Role::Metadata),
    ("{bay}consolidation-skeleton", Role::Metadata),
    ("{bay}operation", Role::Metadata),
    ("{bay}journal.json", Role::Log),
    ("{bay}lock", Role::Lock),
    ("{bay}assembly/**", Role::NoCounterpart),
    ("redirect", Role::NoCounterpart),
    ("clock", Role::NoCounterpart),
    ("exec-probe-<pid>-<n>", Role::NoCounterpart),
    ("**/<name>.tmp~<pid>-<n>", Role::NoCounterpart),
    ("**/<name>.tmp~<pid>-<n>/**", Role::NoCounterpart),
    (".forklift", Role::NoCounterpart),
    (".forklift.new", Role::NoCounterpart),
    (".forklift-server.lock", Role::NoCounterpart),
    ("~/.forkliftconfig", Role::NoCounterpart),
    ("~/.forklift-keys/", Role::NoCounterpart),
    ("<key-id>.key", Role::NoCounterpart),
    ("owners.toml", Role::NoCounterpart),
];

/// The role map's gaps against `census`: each census path it names no role for, and each entry
/// naming a path the census no longer lists.
pub fn role_gaps(census: &Census, roles: &[(&str, Role)]) -> Vec<String> {
    let listed: Vec<&str> = census.rows.iter().flat_map(|row| row.paths.iter().map(String::as_str)).collect();
    let mut gaps: Vec<String> = listed.iter().filter(|path| !roles.iter().any(|(named, _)| named == *path))
        .map(|path| format!("{}: no role", path)).collect();
    gaps.extend(roles.iter().filter(|(named, _)| !listed.contains(named)).map(|(named, _)| format!("stale role: {}", named)));
    gaps.sort();
    gaps.dedup();
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path belongs to the first row that matches it, so the pack and info folders are never
    /// taken for loose objects, nor a branch's lock for the branch.
    ///
    /// Revert: put the loose-object rows first → `objects/pack/` reads as a loose object folder.
    #[test]
    fn a_path_belongs_to_its_most_specific_row() {
        let of = |path: &str| row_of(path).map(|row| row.pattern);
        assert_eq!(of("objects/pack/"), Some("objects/pack/"));
        assert_eq!(of("objects/info/"), Some("objects/info/"));
        assert_eq!(of("objects/ab/"), Some("objects/<xx>/"));
        assert_eq!(of("objects/ab/cdef"), Some("objects/<xx>/<hash>"));
        assert_eq!(of("objects/pack/pack-1.idx"), Some("objects/pack/<name>.idx"));
        assert_eq!(of("refs/heads/main.lock"), Some("**/<name>.lock"));
        assert_eq!(of("index.lock"), Some("**/<name>.lock"));
        assert_eq!(of("refs/heads/main"), Some("refs/heads/**"));
        assert_eq!(of("refs/heads/"), Some("refs/heads/**/"));
        assert_eq!(of("refs/stash"), Some("refs/stash"));
        assert_eq!(of("logs/refs/heads/"), Some("logs/**/"));
        assert_eq!(of("logs/HEAD"), Some("logs/**"));
        assert_eq!(of("MERGE_HEAD"), None);
    }

    /// The walk's unclassified paths are exactly those no row matches.
    ///
    /// Revert: classify by any census row instead → `MERGE_HEAD` passes unnamed.
    #[test]
    fn an_unknown_git_path_is_named() {
        let walked: Vec<String> = ["HEAD", "MERGE_HEAD", "objects/", "objects/ab/", "objects/ab/cd", "rr-cache/"].iter().map(|path| path.to_string()).collect();
        assert_eq!(unclassified(&walked), ["MERGE_HEAD", "rr-cache/"]);
    }

    /// A row's damage resolves only to paths that row owns, chosen by the seed.
    ///
    /// Revert: resolve by the pattern alone → `objects/<xx>/` resolves to the pack folder.
    #[test]
    fn a_row_resolves_only_to_its_own_paths() {
        let walked: Vec<String> = ["objects/", "objects/ab/", "objects/info/", "objects/pack/", "objects/cd/"].iter().map(|path| path.to_string()).collect();
        assert_eq!(reached("objects/<xx>/", &walked), ["objects/ab", "objects/cd"]);
        assert_eq!(resolve("objects/<xx>/", &walked, 1).as_deref(), Some("objects/cd"));
        assert_eq!(resolve("packed-refs", &walked, 0), None);
    }

    /// Every census path in the repository's own census has a role, and every role names a path
    /// the census still lists.
    ///
    /// Revert: drop the `parked` entry → `parked: no role`.
    #[test]
    fn every_census_path_has_a_role() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let census = Census::load(&source).expect("the census parses").expect("the repository has a census");
        assert_eq!(role_gaps(&census, CENSUS_ROLES), Vec::<String>::new());

        let without: Vec<(&str, Role)> = CENSUS_ROLES.iter().copied().filter(|(path, _)| *path != "parked").collect();
        assert_eq!(role_gaps(&census, &without), ["parked: no role"]);
        let extra: Vec<(&str, Role)> = CENSUS_ROLES.iter().copied().chain([("gone", Role::Metadata)]).collect();
        assert_eq!(role_gaps(&census, &extra), ["stale role: gone"]);
    }
}
