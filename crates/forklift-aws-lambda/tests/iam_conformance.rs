//! C11 — the IAM conformance test (design memo `2026-07-26-aws-serverless-terraform-reference.md`
//! v3, §5, third specification).
//!
//! Two previous specifications of this test were wrong. The first matched the literal string
//! `self.client.` and missed every call site rustfmt had split across lines (`grep -c
//! 'self\.client\.'` is 0 for `dynamo.rs`). The obvious repair — loosen the pattern to catch a
//! split chain — still only ever checks operations it was told to look for: a genuinely new SDK
//! call is a pattern nobody wrote, so it stays invisible. Fails-closed requires *deriving* the
//! operation set from the code's structure, not matching strings against it — hence `syn`.
//!
//! The mechanism, matching the design doc's five steps:
//!
//! 1. Every `.rs` file under `src/aws/` is parsed (`read_dir`, not a hardcoded list — call
//!    sites moving or a new file appearing cannot make this test silently stop looking).
//! 2. Every `ExprMethodCall` whose receiver is exactly the field access `self.client` is
//!    enumerated; its method name *is* a derived operation. Nothing here is matched against a
//!    list of known op names — a novel operation arrives in the set automatically.
//! 3. Both directions are checked against the union of the two policy JSONs (`iam/*.policy.json`):
//!    every derived op must appear in `op_actions` (below) and its mapped action(s) must be
//!    granted (an unmapped op, or a missing grant, fails); every granted S3/DynamoDB action must
//!    be produced by some derived op (a dead grant fails).
//! 4. The closure invariant: every `self.client` field access anywhere under `src/aws/` — not
//!    just the ones already found as method-call receivers — is counted separately, and must
//!    equal the number consumed in step 2. Aliasing `self.client` into a local, passing it as an
//!    argument, returning it, or `.clone()`-ing it all break this equality (a `.clone()` also
//!    fails independently, as an unmapped op named `clone`). This is what makes the enumeration
//!    fail closed *by construction* rather than by an unverified claim about it.
//! 5. The smuggling guard: the type tokens `aws_sdk_s3::Client` / `aws_sdk_dynamodb::Client` must
//!    appear in `src/` only at the locations named in `sanctioned_client_counts` below. A helper
//!    function taking a client as a parameter is the one route around invariant 4 (it reaches the
//!    client without ever writing `self.client`), so it must go red until it is added there.
//!
//! **Scope.** This test covers only S3 and DynamoDB actions derived from SDK call sites.
//! **KMS is explicitly out of scope** — the KMS grants are deployment-conditional (only present
//! when a CMK is configured) rather than derived from an unconditional SDK call, so they belong
//! in the Terraform/CDK layer, not this crate's policy JSON (§4.1 draws this line explicitly).
//! Do not "fix" this test to also scan for KMS statements; it would have nothing to check them
//! against, since the JSON here deliberately carries none. CloudWatch Logs grants are excluded
//! for the same reason (deployment-conditional, and not the product of any SDK call this crate
//! makes) — this test only ever inspects `s3:*` and `dynamodb:*` actions in the policy JSON.
//!
//! Per N7 in the design doc: this test pins the *union* of the two policies, not the per-role
//! split — an action the verifier needs but that is granted only to the control plane still
//! passes here. Deriving per-binary sets needs call-graph analysis, not a source scan; that gap
//! is reviewer judgement plus Layer 3 (a real-account deploy), not something this test can close.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use syn::visit::Visit;
use syn::{Expr, ExprField, ExprMethodCall, Member, Path as SynPath};

const AWS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/aws");
const SRC_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");

const CONTROL_PLANE_POLICY: &str = include_str!("../iam/control-plane.policy.json");
const VERIFIER_POLICY: &str = include_str!("../iam/verifier.policy.json");

// ---------------------------------------------------------------------------------------
// File enumeration (step 1 / step 5's scope).
// ---------------------------------------------------------------------------------------

/// Every `.rs` file directly under `src/aws/` — the operation-derivation scope (steps 1-4).
/// `read_dir`, not a hardcoded list, so a call site moving to a new file is still found.
fn aws_source_files() -> Vec<PathBuf> {
    let dir = Path::new(AWS_DIR);
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("reading {}: {}", dir.display(), err))
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("rs"))
        .collect();
    files.sort();
    files
}

/// Every `.rs` file anywhere under `src/`, recursively — the smuggling-guard scope (step 5).
fn all_source_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap_or_else(|err| panic!("reading {}: {}", dir.display(), err))
        {
            let path = entry.expect("dir entry").path();

            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }

    let mut files = Vec::new();
    walk(Path::new(SRC_DIR), &mut files);
    files.sort();
    files
}

fn parse_file(path: &Path) -> syn::File {
    let src = fs::read_to_string(path).unwrap_or_else(|err| panic!("reading {}: {}", path.display(), err));
    syn::parse_file(&src).unwrap_or_else(|err| panic!("parsing {}: {}", path.display(), err))
}

// ---------------------------------------------------------------------------------------
// Steps 2 and 4: derive the operation set, and the closure invariant over `self.client`.
// ---------------------------------------------------------------------------------------

/// Whether `field` is exactly the field access `self.client` — the one receiver shape every
/// store's SDK calls go through.
fn is_self_client_field(field: &ExprField) -> bool {
    let Expr::Path(base) = &*field.base else { return false };

    base.path.is_ident("self")
        && matches!(&field.member, Member::Named(ident) if ident == "client")
}

fn is_self_client(expr: &Expr) -> bool {
    matches!(expr, Expr::Field(field) if is_self_client_field(field))
}

#[derive(Default)]
struct ClientCallVisitor {
    /// Every method call whose receiver is exactly `self.client` — the derived op set.
    ops: Vec<String>,
    /// Every `self.client` field access anywhere, consumed or not — the closure invariant's
    /// total. Must equal `ops.len()` once the walk is done.
    field_accesses: usize,
}

impl<'ast> Visit<'ast> for ClientCallVisitor {
    fn visit_expr_field(&mut self, node: &'ast ExprField) {
        if is_self_client_field(node) {
            self.field_accesses += 1;
        }
        syn::visit::visit_expr_field(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        if is_self_client(&node.receiver) {
            self.ops.push(node.method.to_string());
        }
        syn::visit::visit_expr_method_call(self, node);
    }
}

fn derive_client_calls() -> ClientCallVisitor {
    let mut visitor = ClientCallVisitor::default();

    for path in aws_source_files() {
        visitor.visit_file(&parse_file(&path));
    }

    visitor
}

/// The hand-maintained op -> IAM-action map (step 3). Every operation `derive_client_calls`
/// enumerates from `src/aws/` must have an entry here — an operation this map has never heard
/// of is exactly the "novel operation" scenario C11 exists to catch, so it fails as an unmapped
/// op rather than silently passing. KMS actions are never listed here; see the module docs.
fn op_actions() -> BTreeMap<&'static str, &'static [&'static str]> {
    BTreeMap::from([
        // S3 (`aws/s3.rs`).
        ("head_object", &["s3:GetObject"][..]),
        ("get_object", &["s3:GetObject"][..]),
        ("put_object", &["s3:PutObject"][..]),
        ("delete_object", &["s3:DeleteObject"][..]),
        // `copy_object` reads the source (`staging/*`) and writes the destination
        // (`objects/*`) — both actions, per docs/DEPLOYMENT.md's derivation table.
        ("copy_object", &["s3:GetObject", "s3:PutObject"][..]),
        ("list_objects_v2", &["s3:ListBucket"][..]),
        // DynamoDB (`aws/dynamo.rs`).
        ("get_item", &["dynamodb:GetItem"][..]),
        ("put_item", &["dynamodb:PutItem"][..]),
        ("update_item", &["dynamodb:UpdateItem"][..]),
        ("query", &["dynamodb:Query"][..]),
    ])
}

// ---------------------------------------------------------------------------------------
// The policy JSON side of step 3.
// ---------------------------------------------------------------------------------------

/// Every `s3:*` / `dynamodb:*` action granted anywhere in a policy document. CloudWatch Logs
/// and KMS actions (neither derived from an SDK call site) are deliberately not collected —
/// this test has nothing to check them against, and is not meant to.
fn granted_actions(policy_json: &str) -> BTreeSet<String> {
    let doc: serde_json::Value = serde_json::from_str(policy_json).expect("policy JSON parses");
    let statements = doc["Statement"].as_array().expect("Statement is an array");

    let mut actions = BTreeSet::new();

    for statement in statements {
        let these: Vec<String> = match &statement["Action"] {
            serde_json::Value::String(one) => vec![one.clone()],
            serde_json::Value::Array(many) => many
                .iter()
                .map(|item| item.as_str().expect("action is a string").to_string())
                .collect(),
            other => panic!("unexpected Action shape in policy JSON: {:?}", other),
        };

        actions.extend(
            these.into_iter().filter(|action| action.starts_with("s3:") || action.starts_with("dynamodb:")),
        );
    }

    actions
}

/// C11 itself: enumerate `src/aws/`'s derived operations, close over `self.client` (step 4),
/// then check both directions against the union of the two policy JSONs (step 3).
#[test]
fn iam_policies_match_the_derived_sdk_call_set() {
    let derivation = derive_client_calls();

    // Step 4 — the closure invariant. If this is not an equality, some `self.client` access
    // exists that was never enumerated as a method-call receiver above: an alias, a bare
    // pass-through as an argument, a return, anything other than an immediate `.op(...)` call.
    assert_eq!(
        derivation.field_accesses,
        derivation.ops.len(),
        "src/aws/ accesses `self.client` {} time(s), but only {} of those were consumed as the \
         immediate receiver of a method call — something reaches the client without going \
         through an enumerated operation (aliasing into a variable, passing it as an argument, \
         returning it, etc.). That is the exact escape hatch the closure invariant exists to \
         close.",
        derivation.field_accesses,
        derivation.ops.len(),
    );

    let derived_ops: BTreeSet<String> = derivation.ops.into_iter().collect();
    let actions = op_actions();

    // Step 3, direction A (unmapped op): every derived op must be a known operation.
    let unmapped: Vec<&String> = derived_ops.iter().filter(|op| !actions.contains_key(op.as_str())).collect();
    assert!(
        unmapped.is_empty(),
        "src/aws/ calls self.client.<op>() for {:?}, which has no entry in this test's op -> \
         IAM-action map (`op_actions`). A novel operation must be mapped there — and granted in \
         the policy JSON — before it can be added; that is what closes the gap the previous two \
         specifications of this test left open.",
        unmapped,
    );

    let required: BTreeSet<String> =
        derived_ops.iter().flat_map(|op| actions[op.as_str()].iter().map(|a| a.to_string())).collect();

    let granted: BTreeSet<String> = granted_actions(CONTROL_PLANE_POLICY)
        .union(&granted_actions(VERIFIER_POLICY))
        .cloned()
        .collect();

    // Step 3, direction B (missing grant): every action a derived op needs must be granted
    // somewhere (the union of both policies — see the module docs on N7/per-role attribution).
    let missing: Vec<&String> = required.difference(&granted).collect();
    assert!(
        missing.is_empty(),
        "src/aws/'s derived operations require action(s) {:?} that neither policy JSON grants — \
         a missing grant. Add it to iam/control-plane.policy.json and/or \
         iam/verifier.policy.json.",
        missing,
    );

    // Step 3, direction C (dead grant): every granted S3/DynamoDB action must be produced by
    // some derived op, or it is unused and should be removed.
    let dead: Vec<&String> = granted.difference(&required).collect();
    assert!(
        dead.is_empty(),
        "the policy JSON grants action(s) {:?} that no operation derived from src/aws/ produces \
         — a dead grant. Remove it, or add the call site that needs it.",
        dead,
    );
}

// ---------------------------------------------------------------------------------------
// Step 5: the smuggling guard.
// ---------------------------------------------------------------------------------------

/// Whether `path`'s first two segments are exactly `aws_sdk_s3::Client` or
/// `aws_sdk_dynamodb::Client` — a type reference, or the prefix of an associated-function call
/// such as `Client::new(...)`. Trailing segments (`::new`, `::from_conf`) are ignored, so this
/// matches both the bare type and a call through it.
fn client_prefix(path: &SynPath) -> bool {
    let mut segments = path.segments.iter();
    let (Some(first), Some(second)) = (segments.next(), segments.next()) else { return false };

    matches!(
        (first.ident.to_string().as_str(), second.ident.to_string().as_str()),
        ("aws_sdk_s3", "Client") | ("aws_sdk_dynamodb", "Client")
    )
}

#[derive(Default)]
struct ClientTokenVisitor {
    count: usize,
}

impl<'ast> Visit<'ast> for ClientTokenVisitor {
    fn visit_path(&mut self, node: &'ast SynPath) {
        if client_prefix(node) {
            self.count += 1;
        }
        syn::visit::visit_path(self, node);
    }
}

/// Whether a `use` tree imports the bare `Client` leaf from exactly `aws_sdk_s3` or
/// `aws_sdk_dynamodb`. There are zero sanctioned locations for this: every legitimate use in
/// this crate spells the type out in full (`aws_sdk_s3::Client`), so a bare import is itself an
/// unsanctioned route — it would let a later `Client::new(...)` call evade the fully-qualified
/// token match `ClientTokenVisitor` looks for.
fn imports_bare_client(tree: &syn::UseTree, prefix: &[String]) -> bool {
    let is_sanctioned_root = matches!(prefix, [only] if only == "aws_sdk_s3" || only == "aws_sdk_dynamodb");

    match tree {
        syn::UseTree::Path(path) => {
            let mut next = prefix.to_vec();
            next.push(path.ident.to_string());
            imports_bare_client(&path.tree, &next)
        }
        syn::UseTree::Name(name) => name.ident == "Client" && is_sanctioned_root,
        syn::UseTree::Rename(rename) => rename.ident == "Client" && is_sanctioned_root,
        syn::UseTree::Group(group) => group.items.iter().any(|item| imports_bare_client(item, prefix)),
        syn::UseTree::Glob(_) => false,
    }
}

/// The exact count of `aws_sdk_s3::Client` / `aws_sdk_dynamodb::Client` occurrences sanctioned
/// in each file, keyed relative to `src/`. This is an **exact** count, not a floor: a new
/// occurrence added to an already-sanctioned file (e.g. a helper dropped into `s3.rs` that takes
/// a client parameter) must also go red, which a `>=` check would miss.
fn sanctioned_client_counts() -> BTreeMap<&'static str, usize> {
    BTreeMap::from([
        // `S3ObjectStore`'s field + its constructor's parameter.
        ("aws/s3.rs", 2),
        // `DynamoRefStore`'s field + its constructor's parameter.
        ("aws/dynamo.rs", 2),
        // `build_clients`' return type (both client types) + `Client::from_conf` +
        // `Client::new` (s3) + `Client::new` (dynamodb).
        ("aws/config.rs", 5),
        // The control-plane bin's `Context` struct: its two client fields.
        ("bin/control-plane.rs", 2),
    ])
}

#[test]
fn client_types_appear_only_at_sanctioned_locations() {
    let sanctioned = sanctioned_client_counts();
    let mut found: BTreeMap<String, usize> = BTreeMap::new();
    let mut bare_imports: Vec<String> = Vec::new();

    for path in all_source_files() {
        let file = parse_file(&path);
        let rel = path
            .strip_prefix(SRC_DIR)
            .expect("file under src/")
            .to_string_lossy()
            .replace('\\', "/"); // durable path keys use "/" — see the repo's own convention.

        let mut visitor = ClientTokenVisitor::default();
        visitor.visit_file(&file);

        if visitor.count > 0 {
            found.insert(rel.clone(), visitor.count);
        }

        for item in &file.items {
            if let syn::Item::Use(item_use) = item {
                if imports_bare_client(&item_use.tree, &[]) {
                    bare_imports.push(rel.clone());
                }
            }
        }
    }

    assert!(
        bare_imports.is_empty(),
        "found a bare `use aws_sdk_s3::Client` / `use aws_sdk_dynamodb::Client` import in {:?} — \
         every use in this crate must spell the type out in full so this test's token match can \
         see it; a bare import would let a later `Client::new(...)` call evade detection \
         entirely.",
        bare_imports,
    );

    let sanctioned: BTreeMap<String, usize> =
        sanctioned.into_iter().map(|(path, count)| (path.to_string(), count)).collect();

    assert_eq!(
        found, sanctioned,
        "aws_sdk_s3::Client / aws_sdk_dynamodb::Client must appear only at the sanctioned \
         locations in `sanctioned_client_counts` (the two store structs' fields and \
         constructors, aws::config's builders, and the bin entrypoints). A mismatch means either \
         a new call site reaches a client by an unsanctioned route — a helper taking a client \
         parameter is the classic escape from the closure invariant in \
         `iam_policies_match_the_derived_sdk_call_set` — or this allowlist is stale.\n\
         found:      {:?}\n\
         sanctioned: {:?}",
        found, sanctioned,
    );
}
