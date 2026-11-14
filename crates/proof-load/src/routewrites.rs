//! The server route write check: what a traced server wrote to the warehouse it serves, set against
//! the requests the proxy forwarded to it.
//!
//! Every route the version's route table lists is bound here to its write set, or bound as
//! read-only. A set entry is a kind of change and a path template under the host warehouse, filled
//! from the request's own path captures where the route names its target. The server keeps working
//! after it answers, and work its first requests start goes on running on timers, so its background
//! work and its admission of a warehouse are units with their own sets.
//!
//! The proxy forwards one request at a time when the server is traced, and stamps when each was
//! forwarded and when its response had been read to its end. Both stamps and strace's `-ttt` read
//! the one wall clock of the host the run is on. A handler's awaited write is entered after the
//! server read its request, so after `forwarded`, and before the response it was awaited for was
//! written, so before `answered`. A request whose response was never read keeps its window open to
//! the end of the trace.
//!
//! So each successful write on the warehouse must fall in the set of a route whose window holds
//! its stamp, or in the background or admission set: one that falls in none refuses the run, as a
//! set that leaves a write out. A route is covered once a request to it was answered with success
//! and a write in its set, and in neither shared set, fell in that request's window.

use std::collections::{BTreeMap, BTreeSet};

use crate::kill::Write;
use crate::proxy::Exchange;
use crate::routes::{self, Mode, Mount, RouteTable};
use crate::trace::{Kind, Root};
use crate::writeops::is_lock;

/// One change a unit may make: its kind, and a path template relative to the host warehouse's
/// root. A segment may hold `*`, any run of characters but `/`, and `{name}`, the request's
/// capture of that name, percent-decoded; `{name[a..b]}` takes characters `a` to `b` of it, either
/// bound left out for its end. A final `**` stands for one or more segments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    pub path: &'static str,
}

/// What a route writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    /// Nothing of its own: every write in its window must be background's or admission's.
    ReadOnly,
    Writes(&'static [Entry]),
}

/// One route's binding, by the route table's method, path and mount.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteWrites {
    pub method: &'static str,
    pub path: &'static str,
    pub mount: Mount,
    pub bound: Bound,
}

/// One traced serving of a host warehouse: what its proxy logged, and what its server wrote.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Serving {
    pub scenario: String,
    /// The host warehouse's root, absolute.
    pub root: String,
    pub exchanges: Vec<Exchange>,
    pub writes: Vec<Write>,
    /// Why its writes are not known in full: an unreadable trace, a call the table cannot place.
    pub unsound: Vec<String>,
}

/// `text` with each `%XX` escape decoded, as the server's router decodes a path capture; one
/// that does not decode to UTF-8 is kept as written.
fn decoded(text: &str) -> String {
    percent_encoding::percent_decode_str(text).decode_utf8().map(|text| text.into_owned()).unwrap_or_else(|_| text.to_string())
}

/// A piece of a compiled template.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Piece {
    Literal(char),
    /// Any run of characters but `/`, possibly empty.
    Star,
    /// One or more whole segments.
    Rest,
}

/// `template` with the captures filled in, as pieces; None when it names a capture the request
/// does not have.
fn compile(template: &str, captures: &BTreeMap<String, String>) -> Option<Vec<Piece>> {
    let mut pieces = Vec::new();
    let mut rest = template;
    while let Some(next) = rest.chars().next() {
        if let Some(after) = rest.strip_prefix("**") {
            pieces.push(Piece::Rest);
            rest = after;
        } else if next == '*' {
            pieces.push(Piece::Star);
            rest = &rest[1..];
        } else if next == '{' {
            let close = rest.find('}')?;
            let (name, range) = match rest[1..close].split_once('[') {
                Some((name, range)) => (name, Some(range.strip_suffix(']')?)),
                None => (&rest[1..close], None),
            };
            let value: Vec<char> = captures.get(name)?.chars().collect();
            let value = match range {
                None => &value[..],
                Some(range) => {
                    let (from, to) = range.split_once("..")?;
                    let from = if from.is_empty() { 0 } else { from.parse().ok()? };
                    let to = if to.is_empty() { value.len() } else { to.parse().ok()? };
                    value.get(from..to)?
                }
            };
            pieces.extend(value.iter().copied().map(Piece::Literal));
            rest = &rest[close + 1..];
        } else {
            pieces.push(Piece::Literal(next));
            rest = &rest[next.len_utf8()..];
        }
    }
    Some(pieces)
}

/// Whether `path` matches `pieces` whole.
fn fits(pieces: &[Piece], path: &[char]) -> bool {
    match pieces.split_first() {
        None => path.is_empty(),
        Some((Piece::Literal(wanted), more)) => path.first() == Some(wanted) && fits(more, &path[1..]),
        Some((Piece::Star, more)) => {
            let segment = path.iter().position(|c| *c == '/').unwrap_or(path.len());
            (0..=segment).any(|taken| fits(more, &path[taken..]))
        }
        Some((Piece::Rest, more)) => (1..=path.len()).any(|taken| path[taken - 1] != '/' && fits(more, &path[taken..])),
    }
}

/// Whether the change `kind` at `relative` (to the warehouse root) is in `set`, its templates
/// filled from `captures`.
pub fn holds(set: &[Entry], kind: Kind, relative: &str, captures: &BTreeMap<String, String>) -> bool {
    let path: Vec<char> = relative.chars().collect();
    set.iter().any(|entry| entry.kind == kind && compile(entry.path, captures).is_some_and(|pieces| fits(&pieces, &path)))
}

/// A request's window, its route's binding and captures, and whether it was answered with success.
struct Window<'a> {
    exchange: &'a Exchange,
    route: Option<(String, &'a RouteWrites, BTreeMap<String, String>)>,
    from: u64,
    to: u64,
}

/// What the check finds missing over `servings`, served in `mode`: every write on a warehouse that
/// falls in no set its moment allows; every writing route answered with success with no write of
/// its own in any such request's window; every listed route with no binding; and every binding that
/// names no listed route.
pub fn check(table: &RouteTable, mode: Mode, servings: &[Serving], bindings: &[RouteWrites], background: &[Entry], admission: &[Entry]) -> Vec<String> {
    let mut missing = Vec::new();
    let bound = |route: &routes::Route| bindings.iter().find(|binding| binding.method == route.method && binding.path == route.path && binding.mount == route.mount);
    for route in &table.routes {
        if bound(route).is_none() {
            missing.push(format!("{}: no write binding", route.label()));
        }
    }
    for binding in bindings {
        if !table.routes.iter().any(|route| route.method == binding.method && route.path == binding.path && route.mount == binding.mount) {
            missing.push(format!("stale write binding: {} {} is not a listed route at that mount", binding.method, binding.path));
        }
    }
    let none = BTreeMap::new();
    // The writing routes answered with success, and those of them a write of their own credited.
    let mut answered: BTreeSet<String> = BTreeSet::new();
    let mut credited: BTreeSet<String> = BTreeSet::new();
    for serving in servings {
        missing.extend(serving.unsound.iter().map(|reason| format!("{}: {}", serving.scenario, reason)));
        let Ok(root) = Root::of(std::path::Path::new(&serving.root)) else {
            missing.push(format!("{}: the host warehouse {} is gone, so its writes cannot be placed", serving.scenario, serving.root));
            continue;
        };
        let windows: Vec<Window> = serving.exchanges.iter().filter_map(|exchange| {
            let from = exchange.forwarded?;
            let route = table.resolve(mode, &exchange.method, &exchange.path).ok().flatten().and_then(|route| {
                let binding = bound(route)?;
                let captures = routes::captures(&table.template(route, mode)?, &exchange.path)?.into_iter().map(|(name, value)| (name, decoded(&value))).collect();
                Some((route.label(), binding, captures))
            });
            Some(Window { exchange, route, from, to: exchange.answered.unwrap_or(u64::MAX) })
        }).collect();
        for window in &windows {
            if let Some((label, binding, _)) = &window.route {
                if matches!(binding.bound, Bound::Writes(_)) && window.exchange.status.is_some_and(|status| (200..300).contains(&status)) {
                    answered.insert(label.clone());
                }
            }
        }
        for write in &serving.writes {
            if !root.holds(&write.path) {
                continue;
            }
            let relative = root.tokenized(&write.path).trim_start_matches("<root>").trim_start_matches('/').to_string();
            if relative.is_empty() || is_lock(&relative) {
                continue;
            }
            let named = format!("{}: {} {} ({})", serving.scenario, write.kind.label(), relative, write.call);
            let Some(when) = write.when else {
                missing.push(format!("{}: unstamped, so no window can hold it", named));
                continue;
            };
            let shared = holds(background, write.kind, &relative, &none) || holds(admission, write.kind, &relative, &none);
            let open: Vec<&Window> = windows.iter().filter(|window| window.from <= when && when <= window.to).collect();
            let mut placed = shared;
            for window in &open {
                let Some((label, binding, captures)) = &window.route else { continue };
                let Bound::Writes(set) = binding.bound else { continue };
                if holds(set, write.kind, &relative, captures) {
                    placed = true;
                    if !shared && window.exchange.status.is_some_and(|status| (200..300).contains(&status)) {
                        credited.insert(label.clone());
                    }
                }
            }
            if !placed {
                let during = if open.is_empty() {
                    "outside every request".to_string()
                } else {
                    format!("during {}", open.iter().map(|window| format!("{} {}", window.exchange.method, window.exchange.path)).collect::<Vec<_>>().join(", "))
                };
                let unplaced = format!("{}: in no set, written {}", named, during);
                if !missing.contains(&unplaced) {
                    missing.push(unplaced);
                }
            }
        }
    }
    for label in answered.difference(&credited) {
        missing.push(format!("{}: answered with success, and no write in its set fell inside any such request", label));
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::RouteTable;
    use std::path::PathBuf;

    const TABLE: &str = "<!-- mounts:start -->\n| Mount | Serving mode | Prefix |\n|---|---|---|\n\
        | `protocol` | single | `/v1` |\n| `protocol` | multi | `/warehouses/{warehouse}/v1` |\n<!-- mounts:end -->\n\
        <!-- routes:start -->\n| Method | Path | Mount | Serving mode | Access |\n|---|---|---|---|---|\n\
        | `GET` | `/objects/{hash}` | `protocol` | both | authenticated |\n\
        | `PUT` | `/objects/{hash}` | `protocol` | both | authenticated |\n\
        | `POST` | `/pallets/{*name}` | `protocol` | both | authenticated |\n\
        <!-- routes:end -->\n";

    const OBJECT: &[Entry] = &[Entry { kind: Kind::Write, path: ".forklift/objects/{hash}.tmp~*" }, Entry { kind: Kind::Rename, path: ".forklift/objects/{hash}*" }];
    const PALLET: &[Entry] = &[Entry { kind: Kind::Rename, path: ".forklift/pallets/{name}*" }, Entry { kind: Kind::Rename, path: ".forklift/meta/{name[1..]}" }];
    const BINDINGS: &[RouteWrites] = &[
        RouteWrites { method: "GET", path: "/objects/{hash}", mount: Mount::Protocol, bound: Bound::ReadOnly },
        RouteWrites { method: "PUT", path: "/objects/{hash}", mount: Mount::Protocol, bound: Bound::Writes(OBJECT) },
        RouteWrites { method: "POST", path: "/pallets/{*name}", mount: Mount::Protocol, bound: Bound::Writes(PALLET) },
    ];
    const BACKGROUND: &[Entry] = &[Entry { kind: Kind::Remove, path: ".forklift/bundles/**" }];

    fn table() -> RouteTable {
        RouteTable::parse(TABLE, PathBuf::from("x")).unwrap().unwrap()
    }

    fn exchange(method: &str, path: &str, status: u16, forwarded: u64, answered: Option<u64>) -> Exchange {
        Exchange { index: 0, method: method.into(), path: path.into(), status: Some(status), fault: None, forwarded: Some(forwarded), answered }
    }

    fn write(root: &str, kind: Kind, relative: &str, when: u64) -> Write {
        Write { path: format!("{}/{}", root, relative), kind, when: Some(when), call: "x".into() }
    }

    /// A template's captures, stars and rest match as they say, and a capture the request lacks
    /// matches nothing.
    ///
    /// Revert: let `*` cross a `/` → `a/b/c` matches `a/*`; fill a capture as a glob → `{hash}`
    /// matches any name.
    #[test]
    fn templates_match_what_they_name() {
        let captures: BTreeMap<String, String> = [("hash".to_string(), "ab12".to_string()), ("name".to_string(), "feature/x".to_string())].into();
        let fit = |template: &'static str, path: &str| holds(&[Entry { kind: Kind::Write, path: template }], Kind::Write, path, &captures);
        assert!(fit("objects/{hash}", "objects/ab12"));
        assert!(!fit("objects/{hash}", "objects/ab13"));
        assert!(fit("objects/{hash}.tmp~*", "objects/ab12.tmp~41-0"));
        assert!(fit("a/*", "a/b"));
        assert!(!fit("a/*", "a/b/c"));
        assert!(fit("a/**", "a/b/c"));
        assert!(!fit("a/**", "a/"));
        assert!(fit("pallets/{name}", "pallets/feature/x"));
        assert!(!fit("objects/{missing}", "objects/ab12"));
        assert!(fit("objects/{hash[..2]}/{hash[2..]}", "objects/ab/12"));
        assert!(!fit("objects/{hash[..2]}/{hash[2..]}", "objects/ab/13"));
        assert!(fit("meta/{name[1..5]}", "meta/eatu"));
        assert!(!fit("objects/{hash[3..9]}", "objects/2"), "a range past the capture's end matches nothing");
        assert_eq!(decoded("%40office"), "@office");
        assert_eq!(decoded("a%2Fb%zz%"), "a/b%zz%");
        assert!(!holds(&[Entry { kind: Kind::Rename, path: "a/*" }], Kind::Write, "a/b", &captures), "a kind the entry does not name");
    }

    /// A write in its route's set inside that request's window covers the route; one outside every
    /// window, or in another route's window, or in a read-only route's, refuses; a shared set's write
    /// is placed and credits nothing; a request never answered keeps its window open.
    ///
    /// Revert: credit a write by timing alone → the object write in the pallet window covers PUT
    /// /objects/{hash}; place a write by its route set without its captures → a write of another
    /// object's name is credited; close an unanswered window at its forwarding → the late write
    /// after the cut refuses; drop a serving's unsound reasons → an unplaceable write passes
    /// unseen; match captures as sent → `%40office` never names `meta/office`.
    #[test]
    fn writes_are_placed_by_window_and_set() {
        let root = std::env::temp_dir().join(format!("routewrites-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap().to_string_lossy().into_owned();
        let serving = |exchanges: Vec<Exchange>, writes: Vec<Write>| Serving { scenario: "s".into(), root: root.clone(), exchanges, writes, unsound: vec![] };
        let check = |servings: &[Serving]| check(&table(), Mode::Single, servings, BINDINGS, BACKGROUND, &[]);

        let covered = serving(
            vec![exchange("PUT", "/v1/objects/ab", 201, 100, Some(200)), exchange("POST", "/v1/pallets/main", 204, 300, Some(400)), exchange("GET", "/v1/objects/ab", 200, 500, Some(600))],
            vec![
                write(&root, Kind::Write, ".forklift/objects/ab.tmp~7-0", 150), write(&root, Kind::Rename, ".forklift/objects/ab", 160),
                write(&root, Kind::Rename, ".forklift/pallets/main.tmp~7-1", 350), write(&root, Kind::Remove, ".forklift/bundles/old", 550),
                write(&root, Kind::Remove, ".forklift/bundles/older", 700), write(&root, Kind::Write, ".forklift/lock", 800),
            ],
        );
        assert_eq!(check(std::slice::from_ref(&covered)), Vec::<String>::new());

        let mut astray = covered.clone();
        astray.writes = vec![
            write(&root, Kind::Write, ".forklift/objects/ab.tmp~7-0", 350), write(&root, Kind::Rename, ".forklift/objects/cd", 160),
            write(&root, Kind::Rename, ".forklift/pallets/main", 350), write(&root, Kind::Write, ".forklift/objects/ab.tmp~7-2", 550),
            write(&root, Kind::Write, ".forklift/stray", 900),
        ];
        assert_eq!(check(&[astray]), vec![
            "s: write .forklift/objects/ab.tmp~7-0 (x): in no set, written during POST /v1/pallets/main".to_string(),
            "s: rename .forklift/objects/cd (x): in no set, written during PUT /v1/objects/ab".to_string(),
            "s: write .forklift/objects/ab.tmp~7-2 (x): in no set, written during GET /v1/objects/ab".to_string(),
            "s: write .forklift/stray (x): in no set, written outside every request".to_string(),
            "PUT /objects/{hash}: answered with success, and no write in its set fell inside any such request".to_string(),
        ]);

        // A shared set's write in the route's own set credits nothing.
        let shared = serving(vec![exchange("PUT", "/v1/objects/ab", 201, 100, Some(200))], vec![write(&root, Kind::Remove, ".forklift/bundles/x", 150)]);
        let check_shared = check(&[shared]);
        assert_eq!(check_shared, vec!["PUT /objects/{hash}: answered with success, and no write in its set fell inside any such request".to_string()]);
        let overlapping: &[Entry] = &[Entry { kind: Kind::Rename, path: ".forklift/objects/**" }];
        let both = serving(vec![exchange("PUT", "/v1/objects/ab", 201, 100, Some(200))], vec![write(&root, Kind::Rename, ".forklift/objects/ab", 150)]);
        assert_eq!(super::check(&table(), Mode::Single, &[both], BINDINGS, overlapping, &[]),
            vec!["PUT /objects/{hash}: answered with success, and no write in its set fell inside any such request".to_string()]);

        // A capture is matched decoded, as the server reads it.
        let meta = serving(vec![exchange("POST", "/v1/pallets/%40office", 204, 100, Some(200))], vec![write(&root, Kind::Rename, ".forklift/meta/office", 150)]);
        assert_eq!(check(&[meta]), Vec::<String>::new());

        // A serving whose writes are not known in full refuses, naming why.
        let mut untold = serving(vec![], vec![]);
        untold.unsound = vec!["the server's trace: unplaced".into()];
        assert_eq!(check(&[untold]), vec!["s: the server's trace: unplaced".to_string()]);

        // A request whose response was never read keeps its window open.
        let cut = serving(vec![exchange("POST", "/v1/pallets/main", 204, 100, None)], vec![write(&root, Kind::Rename, ".forklift/pallets/main", 5_000)]);
        assert_eq!(check(&[cut]), Vec::<String>::new());
        #[allow(clippy::disallowed_methods, reason = "the test's own folder")]
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every listed route needs a binding, and a binding naming no listed route is stale.
    ///
    /// Revert: skip the binding sweep → a route added to the table is never asked for its set.
    #[test]
    fn every_route_is_bound() {
        assert_eq!(check(&table(), Mode::Single, &[], &BINDINGS[1..], BACKGROUND, &[]), vec!["GET /objects/{hash}: no write binding".to_string()]);
        let stale = [BINDINGS[0], BINDINGS[1], BINDINGS[2], RouteWrites { method: "GET", path: "/gone", mount: Mount::Root, bound: Bound::ReadOnly }];
        assert_eq!(check(&table(), Mode::Single, &[], &stale, BACKGROUND, &[]), vec!["stale write binding: GET /gone is not a listed route at that mount".to_string()]);
    }
}
