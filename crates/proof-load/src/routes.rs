//! The routes of the version under test: the route and mount tables in that version's
//! `docs/format/REMOTE_PROTOCOL.md`, which a forklift-server test holds to the list its router
//! registers from.
//!
//! As with the census, they are read from the source tree the harness is given, never from its
//! own checkout. A tree with no route table is a result ("not run"), never a fallback; a table that
//! is present but cannot be read is an error, never a guess.
//!
//! The route check matches every exchange the proxy logged while the scenarios were built to the
//! route that answers it. Each listed route must have been answered with success at least once, or
//! carry a named exemption; an exchange that matches no listed route is reported as well, since it
//! means the table and the server disagree.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::proxy::Exchange;

/// Where the route tables live inside a source tree.
pub const PROTOCOL_DOCUMENT: &str = "docs/format/REMOTE_PROTOCOL.md";
const ROUTES: (&str, &str) = ("<!-- routes:start -->", "<!-- routes:end -->");
const MOUNTS: (&str, &str) = ("<!-- mounts:start -->", "<!-- mounts:end -->");
const ROUTES_HEADER: &str = "| Method | Path | Mount | Serving mode | Access |";
const MOUNTS_HEADER: &str = "| Mount | Serving mode | Prefix |";
const METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "DELETE", "PATCH", "OPTIONS"];

/// Where a route is mounted: as written, or under its serving mode's protocol prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mount {
    Root,
    Protocol,
}

/// How a server serves: one warehouse (`--root`) or a folder of them (`--warehouses`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Single,
    Multi,
}

/// The serving modes a route is registered in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Modes {
    Both,
    Multi,
}

impl Modes {
    pub fn includes(self, mode: Mode) -> bool {
        matches!((self, mode), (Modes::Both, _) | (Modes::Multi, Mode::Multi))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Access {
    Authenticated,
    Public,
}

/// One row of the route table.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct Route {
    pub method: String,
    /// The path as the table writes it, before any mount prefix.
    pub path: String,
    pub mount: Mount,
    pub modes: Modes,
    pub access: Access,
}

impl Route {
    /// The route as a check names it: `METHOD path`.
    pub fn label(&self) -> String {
        format!("{} {}", self.method, self.path)
    }
}

/// The route and mount tables of one version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteTable {
    pub routes: Vec<Route>,
    /// The protocol prefix of each serving mode.
    pub single: String,
    pub multi: String,
    /// The document they were read from, for the report.
    pub source: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RoutesError {
    Unreadable { path: PathBuf, reason: String },
    Malformed { path: PathBuf, reason: String },
}

impl fmt::Display for RoutesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RoutesError::Unreadable { path, reason } => write!(f, "cannot read the route table at {}: {}", path.display(), reason),
            RoutesError::Malformed { path, reason } => write!(f, "the route table at {} cannot be read: {}", path.display(), reason),
        }
    }
}

impl std::error::Error for RoutesError {}

/// A route the check does not require a successful answer of, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exemption {
    pub method: &'static str,
    /// The path and mount as the route table writes them.
    pub path: &'static str,
    pub mount: Mount,
    pub reason: &'static str,
}

impl Exemption {
    pub fn names(&self, route: &Route) -> bool {
        self.method == route.method && self.path == route.path && self.mount == route.mount
    }
}

/// The rows between `markers`: `Ok(None)` when neither marker is there.
fn table<'a>(document: &'a str, markers: (&str, &str), header: &str) -> Result<Option<Vec<Vec<&'a str>>>, String> {
    let (start, end) = match (document.find(markers.0), document.find(markers.1)) {
        (None, None) => return Ok(None),
        (Some(start), Some(end)) if start < end => (start + markers.0.len(), end),
        _ => return Err(format!("its {} and {} markers do not enclose a table", markers.0, markers.1)),
    };
    let mut lines = document[start..end].lines().map(str::trim).filter(|line| !line.is_empty());
    let found = lines.next().ok_or_else(|| format!("the table after {} is empty", markers.0))?;
    if found != header {
        return Err(format!("unexpected header {:?} after {}", found, markers.0));
    }
    let columns = header.trim_matches('|').split('|').count();
    lines.next().filter(|line| line.starts_with("|---")).ok_or_else(|| format!("the table after {} has no separator row", markers.0))?;
    let mut rows = Vec::new();
    for line in lines {
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if cells.len() != columns {
            return Err(format!("a row has {} cells, not {}: {}", cells.len(), columns, line));
        }
        rows.push(cells);
    }
    if rows.is_empty() {
        return Err(format!("the table after {} has no rows", markers.0));
    }
    Ok(Some(rows))
}

/// A cell that is one backticked item.
fn quoted<'a>(cell: &'a str, line: &[&str]) -> Result<&'a str, String> {
    cell.strip_prefix('`').and_then(|inner| inner.strip_suffix('`')).filter(|inner| !inner.is_empty() && !inner.contains('`'))
        .ok_or_else(|| format!("{:?} is not one backticked item, in: {}", cell, line.join(" | ")))
}

impl RouteTable {
    /// Reads the route tables of the source tree at `tree`. `Ok(None)` means the version has no
    /// route table: the document or its route markers are absent. Anything present but unreadable
    /// is an error.
    pub fn load(tree: &Path) -> Result<Option<RouteTable>, RoutesError> {
        let path = tree.join(PROTOCOL_DOCUMENT);
        let document = match std::fs::read_to_string(&path) {
            Ok(document) => document,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(RoutesError::Unreadable { path, reason: error.to_string() }),
        };
        RouteTable::parse(&document, path)
    }

    /// Parses a protocol document. `Ok(None)` when it has no route markers; route markers without
    /// the mount table are an error, since a protocol route cannot be placed without it.
    pub fn parse(document: &str, source: PathBuf) -> Result<Option<RouteTable>, RoutesError> {
        let malformed = |reason: String| RoutesError::Malformed { path: source.clone(), reason };
        let Some(rows) = table(document, ROUTES, ROUTES_HEADER).map_err(malformed)? else {
            return Ok(None);
        };
        let mounts = table(document, MOUNTS, MOUNTS_HEADER).map_err(malformed)?
            .ok_or_else(|| malformed("it has a route table and no mount table".to_string()))?;

        let (mut single, mut multi) = (None, None);
        for cells in &mounts {
            if quoted(cells[0], cells).map_err(malformed)? != "protocol" {
                return Err(malformed(format!("a mount row names a mount other than protocol: {}", cells.join(" | "))));
            }
            let prefix = quoted(cells[2], cells).map_err(malformed)?.to_string();
            if !prefix.starts_with('/') || prefix.ends_with('/') {
                return Err(malformed(format!("the prefix {:?} does not start with / and end without one", prefix)));
            }
            let slot = match cells[1] {
                "single" => &mut single,
                "multi" => &mut multi,
                other => return Err(malformed(format!("unknown serving mode {:?} in the mount table", other))),
            };
            if slot.replace(prefix).is_some() {
                return Err(malformed(format!("the mount table lists the {} prefix twice", cells[1])));
            }
        }
        let (Some(single), Some(multi)) = (single, multi) else {
            return Err(malformed("the mount table does not give both a single and a multi prefix".to_string()));
        };

        let mut routes: Vec<Route> = Vec::new();
        for cells in &rows {
            let method = quoted(cells[0], cells).map_err(malformed)?;
            if !METHODS.contains(&method) {
                return Err(malformed(format!("unknown method {:?}", method)));
            }
            let path = quoted(cells[1], cells).map_err(malformed)?;
            if !path.starts_with('/') {
                return Err(malformed(format!("the path {:?} does not start with /", path)));
            }
            let mount = match quoted(cells[2], cells).map_err(malformed)? {
                "root" => Mount::Root,
                "protocol" => Mount::Protocol,
                other => return Err(malformed(format!("unknown mount {:?}", other))),
            };
            let modes = match cells[3] {
                "both" => Modes::Both,
                "multi" => Modes::Multi,
                other => return Err(malformed(format!("unknown serving mode {:?} in the route table", other))),
            };
            let access = match cells[4] {
                "authenticated" => Access::Authenticated,
                "public" => Access::Public,
                other => return Err(malformed(format!("unknown access {:?}", other))),
            };
            let route = Route { method: method.to_string(), path: path.to_string(), mount, modes, access };
            if routes.iter().any(|listed| listed.method == route.method && listed.path == route.path && listed.mount == route.mount) {
                return Err(malformed(format!("the route {} is listed twice", route.label())));
            }
            routes.push(route);
        }
        Ok(Some(RouteTable { routes, single, multi, source }))
    }

    /// The full path template `route` answers at in `mode`, or None when it is not registered
    /// in that mode.
    pub fn template(&self, route: &Route, mode: Mode) -> Option<String> {
        if !route.modes.includes(mode) {
            return None;
        }
        Some(match route.mount {
            Mount::Root => route.path.clone(),
            Mount::Protocol => format!("{}{}", match mode { Mode::Single => &self.single, Mode::Multi => &self.multi }, route.path),
        })
    }

    /// The route a request in `mode` is answered by: the matching route with the most literal
    /// segments, as the router prefers a literal over a capture. Err when two match equally.
    pub fn resolve(&self, mode: Mode, method: &str, path: &str) -> Result<Option<&Route>, String> {
        let mut best: Option<(usize, &Route)> = None;
        let mut tied = false;
        for route in self.routes.iter().filter(|route| route.method == method) {
            let Some(template) = self.template(route, mode) else { continue };
            let Some(literals) = matches(&template, path) else { continue };
            match best {
                Some((most, _)) if most > literals => {}
                Some((most, _)) if most == literals => tied = true,
                _ => {
                    best = Some((literals, route));
                    tied = false;
                }
            }
        }
        if tied {
            return Err(format!("{} {} matches more than one listed route equally", method, path));
        }
        Ok(best.map(|(_, route)| route))
    }
}

/// Whether `path` matches `template`, and if so how many of its segments are literal. `{name}`
/// stands for one non-empty segment and a final `{*name}` for one or more.
fn matches(template: &str, path: &str) -> Option<usize> {
    let wanted: Vec<&str> = template.split('/').skip(1).collect();
    let given: Vec<&str> = path.split('/').skip(1).collect();
    if !path.starts_with('/') {
        return None;
    }
    let mut literals = 0;
    for (at, segment) in wanted.iter().enumerate() {
        if segment.starts_with("{*") && segment.ends_with('}') {
            let rest = given.get(at..)?;
            return (!rest.is_empty() && !rest.iter().all(|part| part.is_empty())).then_some(literals);
        }
        let part = given.get(at)?;
        if segment.starts_with('{') && segment.ends_with('}') {
            if part.is_empty() {
                return None;
            }
        } else if segment == part {
            literals += 1;
        } else {
            return None;
        }
    }
    (given.len() == wanted.len()).then_some(literals)
}

/// What the route check finds missing, over the exchanges logged in `mode`: every listed route no
/// exchange was answered with success at and no exemption covers; every exemption that names no
/// listed route, or a route an exchange was answered at after all; and every exchange that matches
/// no listed route, or more than one.
pub fn check(table: &RouteTable, mode: Mode, exchanges: &[Exchange], exemptions: &[Exemption]) -> Vec<String> {
    let mut missing = Vec::new();
    let mut answered: Vec<&Route> = Vec::new();
    for exchange in exchanges {
        match table.resolve(mode, &exchange.method, &exchange.path) {
            Ok(Some(route)) => {
                if exchange.status.is_some_and(|status| (200..300).contains(&status)) && !answered.contains(&route) {
                    answered.push(route);
                }
            }
            Ok(None) => {
                let status = exchange.status.map_or("no answer".to_string(), |status| status.to_string());
                let unmatched = format!("{} {} ({}) matches no listed route", exchange.method, exchange.path, status);
                if !missing.contains(&unmatched) {
                    missing.push(unmatched);
                }
            }
            Err(ambiguous) => {
                if !missing.contains(&ambiguous) {
                    missing.push(ambiguous);
                }
            }
        }
    }
    for route in &table.routes {
        let exempt = exemptions.iter().find(|exemption| exemption.names(route));
        match (answered.contains(&route), exempt) {
            (true, Some(_)) => missing.push(format!("stale route exemption: {} was answered with success", route.label())),
            (false, None) if !route.modes.includes(mode) => missing.push(format!("{}: not served in the mode the scenarios serve, and no exemption names it", route.label())),
            (false, None) => missing.push(format!("{}: no scenario's request was answered with success", route.label())),
            _ => {}
        }
    }
    for exemption in exemptions {
        if !table.routes.iter().any(|route| exemption.names(route)) {
            missing.push(format!("stale route exemption: {} {} is not a listed route at that mount", exemption.method, exemption.path));
        }
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "<!-- mounts:start -->\n| Mount | Serving mode | Prefix |\n|---|---|---|\n\
        | `protocol` | single | `/v1` |\n| `protocol` | multi | `/warehouses/{warehouse}/v1` |\n<!-- mounts:end -->\n\
        <!-- routes:start -->\n| Method | Path | Mount | Serving mode | Access |\n|---|---|---|---|---|\n\
        | `GET` | `/warehouse` | `protocol` | both | authenticated |\n\
        | `POST` | `/objects/missing` | `protocol` | both | authenticated |\n\
        | `GET` | `/objects/{hash}` | `protocol` | both | authenticated |\n\
        | `GET` | `/objects/missing` | `protocol` | both | authenticated |\n\
        | `POST` | `/pallets/{*name}` | `protocol` | both | authenticated |\n\
        | `PUT` | `/warehouses/{warehouse}` | `root` | multi | authenticated |\n\
        | `GET` | `/healthz` | `root` | both | public |\n\
        <!-- routes:end -->\n";

    fn sample() -> RouteTable {
        RouteTable::parse(SAMPLE, PathBuf::from("sample")).unwrap().unwrap()
    }

    fn exchange(method: &str, path: &str, status: u16) -> Exchange {
        Exchange { index: 0, method: method.to_string(), path: path.to_string(), status: Some(status), fault: None }
    }

    /// The tables parse into routes and both prefixes; a document without route markers has no
    /// table, and one whose table is present but wrong is refused, never guessed at.
    ///
    /// Revert: return None for a malformed row → a renamed column or an unknown mode would be read
    /// as no table, and the run would say "not run" instead of failing.
    #[test]
    fn tables_parse_and_malformed_ones_refuse() {
        let table = sample();
        assert_eq!(table.routes.len(), 7);
        assert_eq!((table.single.as_str(), table.multi.as_str()), ("/v1", "/warehouses/{warehouse}/v1"));
        assert_eq!(table.routes[5], Route { method: "PUT".into(), path: "/warehouses/{warehouse}".into(), mount: Mount::Root, modes: Modes::Multi, access: Access::Authenticated });
        assert_eq!(RouteTable::parse("no tables here", PathBuf::from("x")), Ok(None));

        for broken in [
            SAMPLE.replace("| Access |", "| Auth |"),
            SAMPLE.replace("| both | public |", "| sometimes | public |"),
            SAMPLE.replace("`root` | multi", "`elsewhere` | multi"),
            SAMPLE.replace("`GET` | `/healthz`", "`FETCH` | `/healthz`"),
            SAMPLE.replace("| `protocol` | multi | `/warehouses/{warehouse}/v1` |\n", ""),
            SAMPLE.replace("<!-- mounts:end -->", ""),
            SAMPLE.replace("<!-- mounts:start -->", "").replace("<!-- mounts:end -->", ""),
            SAMPLE.replace("| `GET` | `/healthz` | `root` | both | public |", "| `GET` | `/warehouse` | `protocol` | both | authenticated |"),
            SAMPLE.replace("`/healthz`", "healthz"),
            SAMPLE.replace("| `GET` | `/healthz` | `root` | both | public |", "| `GET` | `/healthz` | `root` | both | public |\n| `GET` | `/healthz` | `root` | both | authenticated |"),
        ] {
            assert!(matches!(RouteTable::parse(&broken, PathBuf::from("x")), Err(RoutesError::Malformed { .. })), "{}", broken);
        }
    }

    /// A request resolves to its route under its mode's prefix: a capture takes one non-empty
    /// segment, a final wildcard one or more, a literal beats a capture, and a multi-only route
    /// does not answer in single mode.
    ///
    /// Revert: let a capture match an empty or a multi-segment place → `/v1/objects/` and
    /// `/v1/objects/a/b` resolve to GET /objects/{hash}; drop the literal preference → GET
    /// /v1/objects/missing is ambiguous.
    #[test]
    fn requests_resolve_to_their_route() {
        let table = sample();
        let resolved = |mode, method, path| table.resolve(mode, method, path).unwrap().map(Route::label);
        assert_eq!(resolved(Mode::Single, "GET", "/v1/objects/ab12"), Some("GET /objects/{hash}".into()));
        assert_eq!(resolved(Mode::Single, "GET", "/v1/objects/missing"), Some("GET /objects/missing".into()));
        assert_eq!(resolved(Mode::Single, "GET", "/v1/objects/"), None);
        assert_eq!(resolved(Mode::Single, "GET", "/v1/objects/a/b"), None);
        assert_eq!(resolved(Mode::Single, "POST", "/v1/pallets/feature/x"), Some("POST /pallets/{*name}".into()));
        assert_eq!(resolved(Mode::Single, "POST", "/v1/pallets/"), None);
        assert_eq!(resolved(Mode::Single, "GET", "/healthz"), Some("GET /healthz".into()));
        assert_eq!(resolved(Mode::Single, "GET", "/warehouse"), None);
        assert_eq!(resolved(Mode::Single, "PUT", "/warehouses/alpha"), None);
        assert_eq!(resolved(Mode::Multi, "PUT", "/warehouses/alpha"), Some("PUT /warehouses/{warehouse}".into()));
        assert_eq!(resolved(Mode::Multi, "GET", "/warehouses/alpha/v1/warehouse"), Some("GET /warehouse".into()));
        assert_eq!(resolved(Mode::Multi, "GET", "/v1/warehouse"), None);
    }

    /// Every route needs a success or an exemption; a failure answer does not count, a route the
    /// checked mode does not serve says so, an exemption that is no longer needed or names no
    /// route is stale, and a request no route matches is reported.
    ///
    /// Revert: count any answer → a 404 at GET /objects/{hash} would cover it; skip unmatched
    /// exchanges → a client calling a path the table never lists goes unseen.
    #[test]
    fn every_route_needs_a_success_or_an_exemption() {
        let table = sample();
        let exemptions = [
            Exemption { method: "PUT", path: "/warehouses/{warehouse}", mount: Mount::Root, reason: "multi mode" },
            Exemption { method: "GET", path: "/objects/missing", mount: Mount::Protocol, reason: "not sent" },
        ];
        let all = [
            exchange("GET", "/v1/warehouse", 200), exchange("POST", "/v1/objects/missing", 200),
            exchange("GET", "/v1/objects/ab", 200), exchange("POST", "/v1/pallets/main", 204), exchange("GET", "/healthz", 200),
        ];
        assert_eq!(check(&table, Mode::Single, &all, &exemptions), Vec::<String>::new());

        let mut failed = all.to_vec();
        failed[2].status = Some(404);
        failed.push(exchange("GET", "/v1/nowhere", 404));
        failed.push(exchange("GET", "/v1/objects/missing", 200));
        assert_eq!(check(&table, Mode::Single, &failed, &[Exemption { method: "GET", path: "/gone", mount: Mount::Root, reason: "x" }]), vec![
            "GET /v1/nowhere (404) matches no listed route".to_string(),
            "GET /objects/{hash}: no scenario's request was answered with success".to_string(),
            "PUT /warehouses/{warehouse}: not served in the mode the scenarios serve, and no exemption names it".to_string(),
            "stale route exemption: GET /gone is not a listed route at that mount".to_string(),
        ]);
        assert_eq!(check(&table, Mode::Single, &failed[..5], &[exemptions[0], Exemption { method: "GET", path: "/healthz", mount: Mount::Root, reason: "x" }]), vec![
            "GET /objects/{hash}: no scenario's request was answered with success".to_string(),
            "GET /objects/missing: no scenario's request was answered with success".to_string(),
            "stale route exemption: GET /healthz was answered with success".to_string(),
        ]);
        // An exemption names a route at its mount: the same method and path under another mount
        // covers nothing.
        assert_eq!(check(&table, Mode::Single, &all[..4], &[exemptions[0], Exemption { method: "GET", path: "/healthz", mount: Mount::Protocol, reason: "x" }]), vec![
            "GET /objects/missing: no scenario's request was answered with success".to_string(),
            "GET /healthz: no scenario's request was answered with success".to_string(),
            "stale route exemption: GET /healthz is not a listed route at that mount".to_string(),
        ]);
    }
}
