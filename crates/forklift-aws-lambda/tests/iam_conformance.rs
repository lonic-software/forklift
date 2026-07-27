//! C11 — the IAM conformance test (design memo `2026-07-26-aws-serverless-terraform-reference.md`
//! v4, the **fourth** specification).
//!
//! The first three specifications were source scanners of increasing sophistication, and each
//! shipped a hole a scanner cannot see past — all three reproduced against the shipped v3 code
//! before this rewrite:
//!
//! 1. **Macro bodies are invisible to `syn`.** `ExprMacro.tokens` is unparsed, so a call inside
//!    `try_join!`/`stringify!`/any macro escapes both the derived op set and the field-access
//!    closure count at once, and the closure invariant's arithmetic still balances.
//! 2. **A non-`self` receiver escapes everything.** A free function `fn f(store: &S3ObjectStore)
//!    { store.client.delete_object()… }` compiles, derives no op, counts no field access, and
//!    trips no type-token guard — because every one of v3's checks keyed off the literal
//!    receiver shape `self.client`.
//! 3. **The bin route was live in shipped code, not hypothetical.** `build_clients` was `pub`
//!    and returned raw `aws_sdk_s3::Client`/`aws_sdk_dynamodb::Client`; `src/bin/control-plane.rs`
//!    held them as `Context` fields and could call anything the SDK exposes. `src/aws/` was the
//!    only scanned directory, so `context.s3.delete_object()` there shipped green.
//!
//! # The mechanism this version rests on
//!
//! None of the three escapes above depended on anyone failing to think of an operation; they
//! depended on `syn` (or the scanner's own scope) being unable to see a call site at all. v4
//! does not try to out-think the next unseen call site — it makes an unsanctioned call site
//! **impossible to write**, and shrinks the scanner to exactly the part of that claim rustc
//! cannot check for it. Three things compose:
//!
//! 1. **Capability wrappers** ([`S3Ops`](forklift_aws_lambda::aws::S3Ops)/
//!    [`DynamoOps`](forklift_aws_lambda::aws::DynamoOps), `src/aws/s3_ops.rs` /
//!    `src/aws/dynamo_ops.rs`) — a private SDK client behind one inherent impl of
//!    `pub(crate)` one-line delegations to the SDK's own fluent builders. Escape 2 (a helper
//!    with a non-`self` receiver) is now moot: there is no `self.client` shape to key off,
//!    because the *type* of any store's `client` field only offers the sanctioned ops, from
//!    anywhere in the crate, to any receiver. Escape 3 is closed the same way: `build_clients`
//!    returns these wrapper types, and their op methods are invisible to a `[[bin]]` target or
//!    an external test (a different crate), so a bin can hold the capability but not invoke
//!    anything on it directly — seeing `Head`
//!    ([`S3ObjectStore::new`](forklift_aws_lambda::aws::S3ObjectStore::new)/
//!    [`DynamoRefStore::new`](forklift_aws_lambda::aws::DynamoRefStore::new)) is the only thing
//!    it can do with one.
//!
//!    **This is only true if the field is actually private and the ops are actually
//!    `pub(crate)`** — the wrapper's privacy is not a fact about the code that a doc comment can
//!    assert into existence; it is a fact this file must check the same as any other, and PR
//!    #79's review caught exactly that gap: flipping `S3Ops`'s field to `pub` (or an op method
//!    to `pub`) left every test green. [`checks::field_privacy_violations`]/
//!    [`checks::op_visibility_violations`] below close it, with probes reproducing both flips.
//! 2. **A scanner shrunk to what it can be total over.** Escape 1 (a macro swallowing a call)
//!    is closed by making the *derived operation set* the wrapper impl's own method names, not
//!    an AST walk over every call site in `src/aws/` — so there is no code left for a macro to
//!    hide a call inside of. What remains scanned (this file) asserts the wrapper modules
//!    contain zero macro invocations (parse totality — `syn`'s one blind spot, forced to zero
//!    rather than "none we happened to notice"), an item-kind allowlist (only `use`, the one
//!    struct, one inherent impl — any *trait* impl, e.g. `impl Deref<Target = Client>`, is red),
//!    a file-level inner-attribute check (exactly `#![deny(dead_code)]` plus doc comments — an
//!    `#![allow(dead_code)]` added beside it, or the `deny` simply deleted, is red; see
//!    [`checks::file_attr_violations`]), and a per-method shape check (a one-line
//!    `self.0.<name>()` delegation, zero arguments).
//! 3. **rustc's own lints for what rustc already tracks better than a scanner could.**
//!    `#![deny(dead_code)]`, scoped to the two wrapper files (below — **not** on the crate;
//!    `lib.rs` carries only `#![forbid(unsafe_code)]`), turns an op method nothing calls into a
//!    build error — confirmed to fire in this crate's layout before this was relied on, and its
//!    own presence is now itself checked (bullet 2, above), since a scanner that verifies the
//!    field is private but not that the enforcing lint is even switched on is checking the wrong
//!    half of the mechanism. `#![forbid(unsafe_code)]` on the crate (`lib.rs`) rules out the one
//!    route around all of the above that privacy alone cannot stop.
//!
//! # What this file actually checks (and what it does not)
//!
//! * **Field privacy and op-method visibility** — the wrapper struct's tuple field must carry
//!   no `pub` at all, and every non-constructor method must be exactly `pub(crate)` (not `pub`,
//!   not bare-private). This is the mechanism itself, not a nice-to-have: see
//!   [`checks::field_privacy_violations`]/[`checks::op_visibility_violations`].
//! * **Parse totality, item-kind allowlist, file-level attributes, per-method shape,
//!   constructor call allowlist** — over the two wrapper modules only (`s3_ops.rs`,
//!   `dynamo_ops.rs`). See the `checks` module.
//! * **Directions A/B (unmapped op, missing grant)** — every op the wrapper impls expose must
//!   have an entry in [`op_actions`], and every action that entry needs must be granted
//!   somewhere in the union of the two policy JSONs. Unchanged in spirit from v3; the input to
//!   direction A is now the wrapper's method-name surface rather than an AST call-site count.
//! * **Direction C, JSON side (dead grant)** — every `s3:`/`dynamodb:` action either policy
//!   grants must be required by some derived op, kept from v3 (an over-permissioned policy is
//!   still worth catching; nothing in the v4 design doc says to drop it, and dropping it would
//!   be a silent coverage loss, not a design decision I'm willing to make unasked — see the
//!   report for this deviation flagged explicitly).
//! * **Direction C, code side (dead op)** — is **not** re-implemented as a source-level check
//!   here. It is `#![deny(dead_code)]`, scoped to each wrapper file, a compiler error rather
//!   than a test assertion for the dead method itself — but the attribute's own *presence* is
//!   now a checked property of this file ([`checks::file_attr_violations`]), because a lint that
//!   enforces something only for as long as nobody deletes it is not an enforced property.
//! * **The smuggling guard** — `aws_sdk_s3::Client`/`aws_sdk_dynamodb::Client` (matched by *any*
//!   path whose first segment is the crate — resolving a crate-level rename alias first — and
//!   whose last segment is `Client`, not only the exact two-segment form; see
//!   [`smuggling::is_client_path`]'s docs for the two gaps this closes) and a bare/glob `use` of
//!   the type must appear in **zero** files under `src/` other than the two wrapper modules.
//!   Kept from v3, minus the exact position-sensitive `sanctioned_client_counts` map (a token
//!   *count* cannot distinguish "the same sanctioned use, refactored" from "a new, unreviewed
//!   occurrence that happens to keep the total the same" — the file-scope zero/nonzero split is
//!   what actually matters). **Residual, stated plainly**: alias resolution here is one level
//!   deep (a crate-root rename); a file that layers a second alias on top of a first, or
//!   re-exports the type under a new name and imports *that*, is not chased further — this is a
//!   syntactic guard, not real import resolution, and the item-kind/shape checks on the two
//!   wrapper files (not this guard) are what keep the raw type's actual reachability closed.
//! * **Policy-side polarity** — [`granted_actions`] panics on a `Deny` covering an in-scope
//!   action (unconditionally: the union model has no way to represent an exception, so it must
//!   never see one). A `Condition` on an in-scope action is **not** an unconditional panic here
//!   — see the deviation note on [`REVIEWED_CONDITIONED_SIDS`] for the mechanism, and for the
//!   `StagingSweep` case that mechanism used to allowlist and why that condition turned out to be
//!   a real bug (found by Layer 3 against a live account, not by review) rather than a reviewed
//!   narrowing — the allowlist is empty today, but stays live for whatever the next one is.
//!
//! **Residuals — not claimed to be closed here.** `op_actions()`'s op → IAM-action mapping is
//! hand-maintained and not verified *correct* (a typo'd action still passes; that is a review
//! and Layer 3 concern). **Layer 3 has now found exactly such a case** — see the
//! [`REVIEWED_CONDITIONED_SIDS`] doc comment for the full account — and it names a concrete
//! *class* this scanner cannot see by construction, not just one missed action: an IAM action a
//! call site needs for its **error semantics** rather than to perform the operation.
//! `head_object`/`get_object` call `s3:GetObject`, and that is the only action `op_actions` maps
//! them to — but on a *missing* key, S3 also consults `s3:ListBucket` on the bucket resource to
//! decide whether the caller is even allowed to learn the key doesn't exist, and answers `403`
//! instead of the modeled `404` when that grant is absent or conditioned to a prefix the request
//! context can't see. No op → action scan can find this: the op really does only call
//! `GetObject`/`HeadObject`, so the mapping is not "wrong" by this file's own model, it is
//! *incomplete* in a way the model has no field for (an action gating a failure path of a
//! *different* action). Future readers adding a new S3 call should ask not only "what action does
//! this call require to succeed" but "does a *failure* of this call — especially a missing-key
//! `404` — depend on an action this call doesn't itself name." Per-role attribution is not
//! checked — only the union across both policies (an action the verifier alone needs but only the
//! control plane is granted still passes; deriving per-binary sets needs call-graph analysis, not
//! a source scan). `Resource`/`Condition` scoping *correctness* is not evaluated beyond the narrow
//! Deny/Condition polarity check above. And none of this defends against an adversarial committer
//! reaching for `transmute`/raw pointers to reach the private field directly —
//! `#![forbid(unsafe_code)]` (see `lib.rs`) is the crate's answer to that, and it is a different
//! threat model (a mistake, not a committer) than what the wrapper's privacy actually defends
//! against.
//!
//! This test's own claim is the **composition**: rustc privacy (the wrapper's private field and
//! `pub(crate)` ops — itself checked, not assumed) + the zero-macro parse-totality assertion +
//! the default-red item/attribute/file-attribute/constructor-call allowlists, each backed by a
//! committed escape probe in `escape_probes` below. It is not "fails-closed by construction" as
//! an unqualified property — that phrasing is exactly what the first three specifications
//! claimed and did not have.
//!
//! One honest qualifier on the constructor-call allowlist specifically: it matches by bare call
//! *name*, with no receiver or return-type context (`syn` does no type resolution), so a
//! hypothetical unrelated `.build()`/`X::new()` inside a future constructor would be accepted
//! just as readily as the SDK's own. What keeps that meaningful is the *scope* around it — the
//! item-kind allowlist limits a wrapper file to one struct and one impl, and the constructor's
//! body is a handful of lines building exactly one client — not the name match doing type-aware
//! verification it cannot actually do.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use syn::visit::Visit;
use syn::{Expr, ExprCall, ExprMethodCall, Item, ItemImpl, Member, Path as SynPath, Stmt};

const SRC_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
const S3_OPS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/aws/s3_ops.rs");
const DYNAMO_OPS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/aws/dynamo_ops.rs");

const CONTROL_PLANE_POLICY: &str = include_str!("../iam/control-plane.policy.json");
const VERIFIER_POLICY: &str = include_str!("../iam/verifier.policy.json");

/// Every non-constructor op method's name is required to appear here, mapped to the IAM
/// action(s) it needs; see the module docs (direction A/B).
fn op_actions() -> BTreeMap<&'static str, &'static [&'static str]> {
    BTreeMap::from([
        // S3 (`aws/s3_ops.rs`).
        ("head_object", &["s3:GetObject"][..]),
        ("get_object", &["s3:GetObject"][..]),
        ("put_object", &["s3:PutObject"][..]),
        ("delete_object", &["s3:DeleteObject"][..]),
        // `copy_object` reads the source (`staging/*`) and writes the destination
        // (`objects/*`) — both actions, per docs/DEPLOYMENT.md's derivation table.
        ("copy_object", &["s3:GetObject", "s3:PutObject"][..]),
        ("list_objects_v2", &["s3:ListBucket"][..]),
        // DynamoDB (`aws/dynamo_ops.rs`).
        ("get_item", &["dynamodb:GetItem"][..]),
        ("put_item", &["dynamodb:PutItem"][..]),
        ("update_item", &["dynamodb:UpdateItem"][..]),
        ("query", &["dynamodb:Query"][..]),
    ])
}

/// The config-builder calls `S3Ops::build`/`DynamoOps::build` are allowed to make — both
/// `.method(...)` fluent calls and `Type::assoc_fn(...)` path calls (the constructors reach for
/// both shapes: `.force_path_style(...)` is a method call, `aws_sdk_s3::Client::new(...)` is
/// not). A call whose name is not here is red until it is consciously added — the same posture
/// as the wrapper's item-kind allowlist. `"Ok"`/`"Err"` and the wrapper struct's own name (its
/// tuple-struct constructor, e.g. `S3Ops(client)`) are structural Rust, not an SDK call, and are
/// added per-file by [`checks::constructor_call_violations`] rather than listed here.
///
/// Small on purpose: the provider-chain/connector construction that used to live in these
/// constructors (`tls_provider`, `defaults`, `region`, `endpoint_url`, `load`, …) was hoisted
/// into `aws/config.rs::load_shared_config` (the cold-start fix — see that function's docs), so
/// `S3Ops::build`/`DynamoOps::build` now only ever turn an already-resolved `SdkConfig` into a
/// client. **Matched by bare call name only** — see the module docs' closing note on what that
/// does and does not verify.
const CONSTRUCTOR_CALL_ALLOWLIST: &[&str] = &["new", "from", "force_path_style", "build", "from_conf"];

/// `Sid`s whose `Condition` has been reviewed and is a deliberate least-privilege narrowing, not
/// an unrepresentable exception the union model would silently misreport.
///
/// **Correction, not a residual.** This allowlist used to carry `StagingSweep`, which scoped
/// `s3:ListBucket` to the `staging/*` prefix with `StringLike: {s3:prefix: "staging/*"}`. The doc
/// comment here argued that was fine because every derived op needing `s3:ListBucket`
/// (`list_objects_v2`, in `aws/s3.rs::discard_session`) still got exactly what it needed under
/// the condition — an **op → action** argument. It was falsified against a real AWS account
/// (not a review guess): `head_object`/`get_object` on a *missing* key also depend on
/// `s3:ListBucket` being grantable — not to perform the call, but because S3 returns `403`
/// instead of `404` for a missing key when the caller lacks `ListBucket`, and refuses to
/// distinguish "no such key" from "no such bucket" without it (see
/// `crates/forklift-aws-lambda/src/aws/sdk.rs`'s `is_head_object_not_found` docs). Worse: a
/// `ListBucket` grant conditioned on `s3:prefix` can **never** satisfy this, because S3 does not
/// place `s3:prefix` in the authorization request context for `HeadObject`/`GetObject` — only an
/// *unconditional* `ListBucket` on the bucket resource works, confirmed by toggling it one
/// variable at a time against a live stack. So the condition was not merely imprecise about
/// scope, as the previous comment claimed — it was wrong, in a way no op→action scan can see,
/// because the actions it protects are not the action the failing call site names (see the
/// module docs' residuals section for the general class this is).
///
/// The fix was to make both policy JSONs' `ListBucket` grant unconditional
/// (`ListBucketFor404Semantics` in both `iam/control-plane.policy.json` and
/// `iam/verifier.policy.json`), which is why this allowlist is empty rather than retired outright:
/// the mechanism — and its default-red behavior for a *new*, unreviewed `Condition` on an
/// in-scope action — stays intact for whatever the next one turns out to be. The design doc's
/// "panic on any in-scope Condition, unconditionally" reading is no longer a deviation forced by
/// an unfixable JSON shape (the previous comment's "'fix the JSON' was not an available option"
/// is itself now obsolete) — the JSON *was* fixed, once the reasoning that had excused it turned
/// out not to hold.
const REVIEWED_CONDITIONED_SIDS: &[&str] = &[];

// ---------------------------------------------------------------------------------------
// File I/O.
// ---------------------------------------------------------------------------------------

fn read_file(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| panic!("reading {}: {}", path.display(), err))
}

fn parse_source(src: &str) -> syn::File {
    syn::parse_file(src).unwrap_or_else(|err| panic!("parsing source: {}", err))
}

/// Every `.rs` file anywhere under `src/`, recursively — the smuggling guard's scope.
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

// =========================================================================================
// The wrapper-module checks (parse totality, item-kind allowlist, method shape, constructor
// call allowlist, derived op set). Every function here takes a parsed `syn::File` — never a
// path — so the committed escape probes below can feed it synthetic, malformed source.
// =========================================================================================
mod checks {
    use super::*;

    /// Whether `imp` is the sanctioned *inherent* impl of `struct_name` — `imp.trait_.is_none()`
    /// is the check that makes a trait impl (`impl Deref<Target = aws_sdk_s3::Client> for
    /// S3Ops`) fail this, rather than being silently accepted as "an impl of the struct".
    pub fn is_inherent_impl_of(imp: &ItemImpl, struct_name: &str) -> bool {
        imp.trait_.is_none()
            && matches!(&*imp.self_ty, syn::Type::Path(type_path) if type_path.path.is_ident(struct_name))
    }

    /// Parse totality: every `syn::Macro` node anywhere in the file — statement, expression, or
    /// item position. `syn`'s only blind spot is a macro's *tokens*, so the guarantee-bearing
    /// property a wrapper module needs is that this count is exactly zero, not "none we noticed".
    pub fn macro_count(file: &syn::File) -> usize {
        struct MacroVisitor {
            count: usize,
        }

        impl<'ast> Visit<'ast> for MacroVisitor {
            fn visit_macro(&mut self, node: &'ast syn::Macro) {
                self.count += 1;
                syn::visit::visit_macro(self, node);
            }
        }

        let mut visitor = MacroVisitor { count: 0 };
        visitor.visit_file(file);
        visitor.count
    }

    /// Whether `attr` is `#[deny(dead_code)]` (the file-level form, `#![deny(dead_code)]`,
    /// desugars to this same `Attribute` shape attached to the file rather than an item).
    fn is_deny_dead_code(attr: &syn::Attribute) -> bool {
        if !attr.path().is_ident("deny") {
            return false;
        }

        let mut found = false;
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("dead_code") {
                found = true;
            }
            Ok(())
        });

        found
    }

    /// The file-level (`#![...]`) attribute check: a wrapper module's `syn::File::attrs` must be
    /// exactly doc comments plus one `#![deny(dead_code)]` — no more, no fewer. This is a
    /// distinct scan from [`item_kind_violations`]'s attribute check, which only ever inspects
    /// `file.items` and so is structurally blind to inner attributes at the top of the file
    /// (PR #79's review: deleting `#![deny(dead_code)]`, or adding `#![allow(dead_code)]` right
    /// beside it, previously passed every test in this file). Requiring the `deny` to be present
    /// is what makes direction C's enforcement (a compiler error on a dead op method) an
    /// asserted property rather than an attribute nobody is checking is still switched on.
    pub fn file_attr_violations(file: &syn::File) -> Vec<String> {
        let mut violations = Vec::new();
        let mut has_deny_dead_code = false;

        for attr in &file.attrs {
            if attr.path().is_ident("doc") {
                continue;
            }
            if is_deny_dead_code(attr) {
                has_deny_dead_code = true;
                continue;
            }

            let name =
                attr.path().get_ident().map(|ident| ident.to_string()).unwrap_or_else(|| "?".to_string());
            violations.push(format!(
                "file-level attribute `#![{}(...)]` is not allowed here — only \
                 `#![deny(dead_code)]` and doc comments may appear at file scope in a wrapper \
                 module (an `#![allow(dead_code)]` beside the `deny` would otherwise silently \
                 defeat it for every item in the file)",
                name
            ));
        }

        if !has_deny_dead_code {
            violations.push(
                "missing `#![deny(dead_code)]` at file scope — direction C (an op method \
                 nothing calls) relies entirely on this attribute to turn a dead method into a \
                 build error; without it, an unused `pub(crate)` op method compiles silently"
                    .to_string(),
            );
        }

        violations
    }

    fn readable_item_kind(item: &Item) -> &'static str {
        match item {
            Item::Const(_) => "const",
            Item::Enum(_) => "enum",
            Item::ExternCrate(_) => "extern crate",
            Item::Fn(_) => "free fn",
            Item::ForeignMod(_) => "foreign mod",
            Item::Impl(_) => "an impl that is not the sanctioned inherent impl (a trait impl?)",
            Item::Macro(_) => "macro invocation",
            Item::Mod(_) => "mod",
            Item::Static(_) => "static",
            Item::Struct(_) => "a struct that is not the sanctioned one",
            Item::Trait(_) => "trait",
            Item::TraitAlias(_) => "trait alias",
            Item::Type(_) => "type alias",
            Item::Union(_) => "union",
            Item::Use(_) => "use", // unreachable: Use is always allowed, matched before this fn
            _ => "an item kind this allowlist has never seen",
        }
    }

    /// Whether `attr` is exactly `#[derive(Clone, Debug)]` — no more, no fewer, and no other
    /// derive. An allowlist that only checked the *path* `derive` (not its contents) would let
    /// `#[derive(Clone, Debug, SomethingLoadBearing)]` slip through unnoticed.
    fn is_the_one_allowed_derive(attr: &syn::Attribute) -> bool {
        if !attr.path().is_ident("derive") {
            return false;
        }

        let mut idents: Vec<String> = Vec::new();
        let parsed = attr.parse_nested_meta(|meta| {
            if let Some(ident) = meta.path.get_ident() {
                idents.push(ident.to_string());
            }
            Ok(())
        });

        if parsed.is_err() {
            return false;
        }

        idents.sort();
        idents == ["Clone", "Debug"]
    }

    /// Every attribute on `attrs` must be a doc comment (`///`/`//!`, which desugar to
    /// `#[doc = "..."]`) or the one allowed derive. Anything else — `#[allow(dead_code)]` above
    /// all, which would otherwise defeat the file-level `#![deny(dead_code)]` this whole design
    /// leans on for direction C (see [`file_attr_violations`] for the check on that attribute's
    /// own presence) — is red.
    fn attribute_violations(attrs: &[syn::Attribute], label: &str) -> Vec<String> {
        attrs
            .iter()
            .filter(|attr| !attr.path().is_ident("doc") && !is_the_one_allowed_derive(attr))
            .map(|attr| {
                let name =
                    attr.path().get_ident().map(|ident| ident.to_string()).unwrap_or_else(|| "?".to_string());
                format!("{} carries a disallowed attribute `#[{}(...)]`", label, name)
            })
            .collect()
    }

    /// The item-kind allowlist (default-red): a wrapper module may contain only `use` items,
    /// exactly one `struct {struct_name}`, and exactly one sanctioned inherent impl of it — and
    /// neither the struct nor the impl may carry an attribute beyond the one allowed derive and
    /// doc comments. Also checks every method's own attributes for the same reason (a
    /// `#[allow(dead_code)]` on one method, not the whole impl, is the more surgical version of
    /// the same escape).
    pub fn item_kind_violations(file: &syn::File, struct_name: &str) -> Vec<String> {
        let mut violations = Vec::new();
        let mut struct_count = 0usize;
        let mut impl_count = 0usize;

        for item in &file.items {
            match item {
                Item::Use(_) => {}
                Item::Struct(item_struct) if item_struct.ident == struct_name => {
                    struct_count += 1;
                    violations.extend(attribute_violations(
                        &item_struct.attrs,
                        &format!("struct {}", struct_name),
                    ));
                }
                Item::Impl(item_impl) if is_inherent_impl_of(item_impl, struct_name) => {
                    impl_count += 1;
                    violations.extend(attribute_violations(
                        &item_impl.attrs,
                        &format!("impl {}", struct_name),
                    ));

                    for impl_item in &item_impl.items {
                        if let syn::ImplItem::Fn(method) = impl_item {
                            violations.extend(attribute_violations(
                                &method.attrs,
                                &format!("method {}::{}", struct_name, method.sig.ident),
                            ));
                        }
                    }
                }
                other => violations.push(format!(
                    "disallowed item in the wrapper module: {}",
                    readable_item_kind(other)
                )),
            }
        }

        if struct_count != 1 {
            violations.push(format!(
                "expected exactly one `struct {}`, found {}",
                struct_name, struct_count
            ));
        }
        if impl_count != 1 {
            violations.push(format!(
                "expected exactly one sanctioned inherent `impl {}`, found {}",
                struct_name, impl_count
            ));
        }

        violations
    }

    /// A short label for `vis`, for violation messages — deliberately not a full token-stream
    /// rendering (no `quote` dependency needed for three cases).
    fn visibility_label(vis: &syn::Visibility) -> &'static str {
        match vis {
            syn::Visibility::Public(_) => "pub",
            syn::Visibility::Inherited => "private (no visibility modifier)",
            syn::Visibility::Restricted(restricted) if restricted.path.is_ident("crate") => "pub(crate)",
            syn::Visibility::Restricted(_) => "a restricted visibility other than pub(crate) (pub(self)/pub(super)/pub(in ...))",
        }
    }

    fn is_pub_crate(vis: &syn::Visibility) -> bool {
        matches!(vis, syn::Visibility::Restricted(restricted) if restricted.path.is_ident("crate"))
    }

    /// **The mechanism itself.** The wrapper struct's one tuple field must carry no visibility
    /// modifier at all — not `pub`, not `pub(crate)` — because the private field is the entire
    /// reason a caller has to go through a delegation method rather than reaching the SDK client
    /// directly. PR #79's review demonstrated the gap this closes: flipping
    /// `S3Ops(aws_sdk_s3::Client)` to `S3Ops(pub aws_sdk_s3::Client)` left all ten tests green,
    /// and re-opened escape 3 exactly (`ctx.s3.0.delete_bucket()` compiles without ever naming
    /// `aws_sdk_s3::Client`, so the smuggling guard's path-token match never even sees it).
    pub fn field_privacy_violations(file: &syn::File, struct_name: &str) -> Vec<String> {
        for item in &file.items {
            let Item::Struct(item_struct) = item else { continue };
            if item_struct.ident != struct_name {
                continue;
            }

            let syn::Fields::Unnamed(fields) = &item_struct.fields else {
                return vec![format!(
                    "struct {} must be a tuple struct with exactly one unnamed field",
                    struct_name
                )];
            };

            if fields.unnamed.len() != 1 {
                return vec![format!(
                    "struct {} must have exactly one field, found {}",
                    struct_name,
                    fields.unnamed.len()
                )];
            }

            let field = &fields.unnamed[0];
            if !matches!(field.vis, syn::Visibility::Inherited) {
                return vec![format!(
                    "struct {}'s tuple field is not private — found `{}`. The private field is \
                     what forces every access through a `pub(crate)` delegation; any visibility \
                     modifier at all (even `pub(crate)`) lets a caller reach the raw client via \
                     `.0` directly, skipping the delegation (and the operation it names) \
                     entirely.",
                    struct_name,
                    visibility_label(&field.vis)
                )];
            }

            return Vec::new();
        }

        vec![format!("no `struct {}` found to check field privacy on", struct_name)]
    }

    /// **The other half of the mechanism.** Every non-constructor method on the sanctioned
    /// inherent impl must be exactly `pub(crate)` — not `pub` (visible to a `[[bin]]` target or
    /// an external integration test, reopening escape 3 the moment the field is also made
    /// reachable, or even on its own if the field were ever relaxed) and not bare-private
    /// (invisible even to the sibling store module that is supposed to call it, e.g. `aws/s3.rs`
    /// calling `self.client.delete_object()` where `client: S3Ops` — bare-private would break
    /// that from compiling at all, which is a correctness bug, not a security one, but still not
    /// the sanctioned shape). The constructor is exempt: its own visibility does not gate an
    /// operation the way an op method's does.
    pub fn op_visibility_violations(
        file: &syn::File,
        struct_name: &str,
        constructor_name: &str,
    ) -> Vec<String> {
        let mut violations = Vec::new();

        for item in &file.items {
            let Item::Impl(item_impl) = item else { continue };
            if !is_inherent_impl_of(item_impl, struct_name) {
                continue;
            }

            for impl_item in &item_impl.items {
                let syn::ImplItem::Fn(method) = impl_item else { continue };
                let name = method.sig.ident.to_string();

                if name == constructor_name {
                    continue;
                }

                if !is_pub_crate(&method.vis) {
                    violations.push(format!(
                        "{}::{} must be exactly `pub(crate)` — found `{}`. `pub` is directly \
                         callable from a `[[bin]]` target or an external integration test crate \
                         (escape 3, reopened); bare-private is invisible even to the sibling \
                         store module this op exists to serve.",
                        struct_name,
                        name,
                        visibility_label(&method.vis)
                    ));
                }
            }
        }

        violations
    }

    /// Whether `expr` is exactly the field access `self.0` — the wrapper's private tuple field.
    fn is_self_dot_zero(expr: &Expr) -> bool {
        matches!(expr, Expr::Field(field) if matches!(&*field.base, Expr::Path(p) if p.path.is_ident("self"))
            && matches!(&field.member, Member::Unnamed(index) if index.index == 0))
    }

    /// The per-method shape check (direction: every non-constructor method). A method's body
    /// must be exactly one statement, a bare tail expression, which is a zero-argument method
    /// call on `self.0` whose name is exactly the enclosing method's own name — i.e. precisely
    /// `pub(crate) fn foo(&self) -> X { self.0.foo() }` and nothing structurally different.
    pub fn op_method_shape_violations(
        file: &syn::File,
        struct_name: &str,
        constructor_name: &str,
    ) -> Vec<String> {
        let mut violations = Vec::new();

        for item in &file.items {
            let Item::Impl(item_impl) = item else { continue };
            if !is_inherent_impl_of(item_impl, struct_name) {
                continue;
            }

            for impl_item in &item_impl.items {
                let syn::ImplItem::Fn(method) = impl_item else { continue };
                let name = method.sig.ident.to_string();

                if name == constructor_name {
                    continue; // the constructor is exempt from this shape; see below.
                }

                if method.block.stmts.len() != 1 {
                    violations.push(format!(
                        "{}::{} must be a single expression; found {} statements",
                        struct_name,
                        name,
                        method.block.stmts.len()
                    ));
                    continue;
                }

                let Stmt::Expr(expr, None) = &method.block.stmts[0] else {
                    violations.push(format!(
                        "{}::{}'s one statement is not a bare tail expression",
                        struct_name, name
                    ));
                    continue;
                };

                let Expr::MethodCall(call) = expr else {
                    violations.push(format!("{}::{}'s body is not a method call", struct_name, name));
                    continue;
                };

                if call.method != name {
                    violations.push(format!(
                        "{}::{} delegates to `.{}(...)` instead of `.{}(...)`",
                        struct_name, name, call.method, name
                    ));
                }
                if !call.args.is_empty() {
                    violations.push(format!(
                        "{}::{} passes argument(s) to the delegated call",
                        struct_name, name
                    ));
                }
                if !is_self_dot_zero(&call.receiver) {
                    violations.push(format!(
                        "{}::{}'s delegated call is not on `self.0`",
                        struct_name, name
                    ));
                }
            }
        }

        violations
    }

    /// Every call name (`.method(...)` or `Type::assoc_fn(...)`) inside `constructor_name`'s
    /// body that is not on `allowlist` (plus `struct_name`'s own tuple constructor and
    /// `Ok`/`Err`, which are structural Rust, not an SDK call). New config-building code goes
    /// red here until it is consciously allowlisted — the same posture as the item-kind check.
    pub fn constructor_call_violations(
        file: &syn::File,
        struct_name: &str,
        constructor_name: &str,
        allowlist: &[&str],
    ) -> Vec<String> {
        struct CallVisitor {
            names: Vec<String>,
        }

        impl<'ast> Visit<'ast> for CallVisitor {
            fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
                self.names.push(node.method.to_string());
                syn::visit::visit_expr_method_call(self, node);
            }

            fn visit_expr_call(&mut self, node: &'ast ExprCall) {
                if let Expr::Path(path) = &*node.func {
                    if let Some(last) = path.path.segments.last() {
                        self.names.push(last.ident.to_string());
                    }
                }
                syn::visit::visit_expr_call(self, node);
            }
        }

        let mut violations = Vec::new();

        for item in &file.items {
            let Item::Impl(item_impl) = item else { continue };
            if !is_inherent_impl_of(item_impl, struct_name) {
                continue;
            }

            for impl_item in &item_impl.items {
                let syn::ImplItem::Fn(method) = impl_item else { continue };
                if method.sig.ident != constructor_name {
                    continue;
                }

                let mut visitor = CallVisitor { names: Vec::new() };
                visitor.visit_block(&method.block);

                for name in visitor.names {
                    let structural = name == struct_name || name == "Ok" || name == "Err";
                    if !structural && !allowlist.contains(&name.as_str()) {
                        violations.push(format!(
                            "{}::{} calls `{}(...)`, which is not on the config-builder allowlist",
                            struct_name, constructor_name, name
                        ));
                    }
                }
            }
        }

        violations
    }

    /// The derived operation set: simply the sanctioned inherent impl's non-constructor method
    /// names. No call-site walk over `aws/s3.rs`/`aws/dynamo.rs` is needed — rustc's privacy
    /// already guarantees nothing outside this file can name an operation this impl does not
    /// expose (see the module docs).
    pub fn derived_ops(file: &syn::File, struct_name: &str, constructor_name: &str) -> BTreeSet<String> {
        let mut ops = BTreeSet::new();

        for item in &file.items {
            let Item::Impl(item_impl) = item else { continue };
            if !is_inherent_impl_of(item_impl, struct_name) {
                continue;
            }

            for impl_item in &item_impl.items {
                if let syn::ImplItem::Fn(method) = impl_item {
                    let name = method.sig.ident.to_string();
                    if name != constructor_name {
                        ops.insert(name);
                    }
                }
            }
        }

        ops
    }
}

// =========================================================================================
// The smuggling guard: a raw client type, or a bare/glob import of it, in any file other than
// the two wrapper modules.
// =========================================================================================
mod smuggling {
    use super::*;

    /// The two real crate names a client path may resolve to.
    const SANCTIONED_ROOTS: [&str; 2] = ["aws_sdk_s3", "aws_sdk_dynamodb"];

    /// Every crate-root rename alias in `file` — e.g. `use aws_sdk_s3 as sdk;` binds `sdk` to
    /// `aws_sdk_s3`. PR #79's review: `sdk::Client::new(...)` after such a rename evades a path
    /// match keyed on the literal segment text `"aws_sdk_s3"`, since the written path never
    /// contains that text at all. One alias level is resolved (a rename of the crate root
    /// itself); see this module's docs (in the parent file) for the residual this does not chase
    /// further. Scanned with the same full recursive `Visit` as the bare-import check, so an
    /// alias introduced inside a nested `mod` or fn body is not invisible either.
    fn crate_aliases(file: &syn::File) -> BTreeMap<String, &'static str> {
        struct AliasVisitor {
            aliases: BTreeMap<String, &'static str>,
        }

        impl<'ast> Visit<'ast> for AliasVisitor {
            fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
                if let syn::UseTree::Rename(rename) = &node.tree {
                    let original = rename.ident.to_string();
                    if let Some(root) = SANCTIONED_ROOTS.iter().find(|root| **root == original) {
                        self.aliases.insert(rename.rename.to_string(), root);
                    }
                }
                syn::visit::visit_item_use(self, node);
            }
        }

        let mut visitor = AliasVisitor { aliases: BTreeMap::new() };
        visitor.visit_file(file);
        visitor.aliases
    }

    /// Whether `path` resolves (through `aliases`) to a sanctioned crate's `Client` type —
    /// generalized twice over the exact-two-segment match this replaces:
    ///
    /// 1. **Any depth, not exactly two segments** — `aws_sdk_s3::client::Client` (the crate's
    ///    own `pub mod client` submodule, re-exported at the crate root as `aws_sdk_s3::Client`;
    ///    both spellings name the identical type) matches just as surely as the two-segment
    ///    form, because this checks "first segment is the crate, last segment is `Client`", not
    ///    "exactly these two segments".
    /// 2. **A crate-level rename alias** — `path`'s first segment is looked up in `aliases`
    ///    first, so `sdk::Client::new(...)` after `use aws_sdk_s3 as sdk;` resolves to
    ///    `aws_sdk_s3` before the crate-name comparison runs.
    ///
    /// Both gaps were demonstrated against the prior two-segment-exact check; see the escape
    /// probes for both.
    pub fn is_client_path(path: &SynPath, aliases: &BTreeMap<String, &'static str>) -> bool {
        if path.segments.len() < 2 {
            return false;
        }

        let first = path.segments.first().expect("checked len >= 2").ident.to_string();
        let resolved_root = aliases.get(&first).copied().unwrap_or(first.as_str());

        let last_is_client =
            path.segments.last().expect("checked len >= 2").ident == "Client";

        SANCTIONED_ROOTS.contains(&resolved_root) && last_is_client
    }

    /// The count of `aws_sdk_s3`/`aws_sdk_dynamodb` `Client` path tokens anywhere in `file`,
    /// resolving crate-rename aliases and matching any path depth ending in `Client` (see
    /// [`is_client_path`]). Outside the two wrapper modules, this must be exactly zero — not an
    /// exact allowlisted count (v3's `sanctioned_client_counts` was position-blind: swapping a
    /// constructor parameter for a helper parameter kept the total the same and stayed green).
    pub fn client_token_count(file: &syn::File) -> usize {
        let aliases = crate_aliases(file);

        struct TokenVisitor<'a> {
            aliases: &'a BTreeMap<String, &'static str>,
            count: usize,
        }

        impl<'ast> Visit<'ast> for TokenVisitor<'_> {
            fn visit_path(&mut self, node: &'ast SynPath) {
                if is_client_path(node, self.aliases) {
                    self.count += 1;
                }
                syn::visit::visit_path(self, node);
            }
        }

        let mut visitor = TokenVisitor { aliases: &aliases, count: 0 };
        visitor.visit_file(file);
        visitor.count
    }

    /// Whether a `use` tree imports the bare `Client` leaf — or a glob — from underneath a
    /// sanctioned root, at **any** depth (not only the exact one-segment prefix `aws_sdk_s3::`;
    /// `aws_sdk_s3::client::Client` — the crate's own submodule path for the identical type — is
    /// underneath the root just as much as the crate-level re-export is). A bare/renamed import
    /// would let a later `Client::new(...)` evade [`client_token_count`]'s match; a glob is the
    /// same hole (v3's bug: `UseTree::Glob(_) => false` was the literal "unseen -> OK" bucket).
    fn imports_bare_client(tree: &syn::UseTree, prefix: &[String]) -> bool {
        let is_sanctioned_root = matches!(prefix.first(), Some(root) if SANCTIONED_ROOTS.contains(&root.as_str()));

        match tree {
            syn::UseTree::Path(path) => {
                let mut next = prefix.to_vec();
                next.push(path.ident.to_string());
                imports_bare_client(&path.tree, &next)
            }
            syn::UseTree::Name(name) => name.ident == "Client" && is_sanctioned_root,
            syn::UseTree::Rename(rename) => rename.ident == "Client" && is_sanctioned_root,
            syn::UseTree::Group(group) => group.items.iter().any(|item| imports_bare_client(item, prefix)),
            // Flipped from v3's `=> false`: a glob under a sanctioned root imports `Client`
            // (among everything else) just as surely as a named import does.
            syn::UseTree::Glob(_) => is_sanctioned_root,
        }
    }

    /// Whether `file` contains a bare or glob import of `Client` from either sanctioned root,
    /// **anywhere** — a full recursive `Visit` (`visit_item_use`), not a top-level-only scan, so
    /// a `use` nested in an inline `mod { ... }` or inside a function body is not invisible to
    /// this the way v3's `for item in &file.items` was.
    pub fn has_bare_client_import(file: &syn::File) -> bool {
        struct UseVisitor {
            found: bool,
        }

        impl<'ast> Visit<'ast> for UseVisitor {
            fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
                if imports_bare_client(&node.tree, &[]) {
                    self.found = true;
                }
                syn::visit::visit_item_use(self, node);
            }
        }

        let mut visitor = UseVisitor { found: false };
        visitor.visit_file(file);
        visitor.found
    }
}

// =========================================================================================
// Directions A/B/C over the policy JSON, and the Deny/Condition polarity check.
// =========================================================================================

/// Every `s3:*`/`dynamodb:*` action a policy statement grants. Panics on an in-scope `Deny`
/// (unconditionally — the union model has no way to represent an exception) or an in-scope
/// `Condition` on a `Sid` not present in `reviewed_conditioned_sids`.
///
/// **`reviewed_conditioned_sids` is a parameter, not a read of the file-level
/// [`REVIEWED_CONDITIONED_SIDS`] constant** (PR #82 review, finding 4). It used to read that
/// constant directly, which meant that with the constant empty (as it is today — see its own
/// docs), the `&& !REVIEWED_CONDITIONED_SIDS.contains(&sid)` term was true for every input this
/// file's own tests ever exercised, and deleting the term outright still left every test green —
/// the allowlist *mechanism itself* (as opposed to the unconditional-Deny/Condition polarity check
/// around it) was asserted nowhere. Taking the allowlist as an argument lets
/// [`escape_probes::probe_a_reviewed_condition_statement_does_not_panic`] hand in a genuinely
/// non-empty one and assert the no-panic path is real, not just absent from every probe that
/// happened to run.
fn granted_actions(policy_json: &str, reviewed_conditioned_sids: &[&str]) -> BTreeSet<String> {
    let doc: serde_json::Value = serde_json::from_str(policy_json).expect("policy JSON parses");
    let statements = doc["Statement"].as_array().expect("Statement is an array");

    let mut actions = BTreeSet::new();

    for statement in statements {
        let sid = statement["Sid"].as_str().unwrap_or("<no Sid>");
        let effect = statement["Effect"].as_str().expect("Effect is a string");

        let these: Vec<String> = match &statement["Action"] {
            serde_json::Value::String(one) => vec![one.clone()],
            serde_json::Value::Array(many) => many
                .iter()
                .map(|item| item.as_str().expect("action is a string").to_string())
                .collect(),
            other => panic!("unexpected Action shape in policy JSON: {:?}", other),
        };

        let in_scope: Vec<String> =
            these.into_iter().filter(|action| action.starts_with("s3:") || action.starts_with("dynamodb:")).collect();

        if in_scope.is_empty() {
            continue; // out of C11's scope entirely (Logs, KMS) — see the module docs.
        }

        assert_ne!(
            effect,
            "Deny",
            "policy statement {:?} carries `Effect: Deny` on in-scope action(s) {:?} — the union \
             model this test pins (\"is this action granted anywhere\") cannot represent a Deny; \
             unrepresentable input is an error here, not something to silently fold into \
             \"not granted\".",
            sid,
            in_scope,
        );

        if statement.get("Condition").is_some() && !reviewed_conditioned_sids.contains(&sid) {
            panic!(
                "policy statement {:?} carries a `Condition` on in-scope action(s) {:?} that is \
                 not in the reviewed-conditioned-Sids allowlist — the union model records an \
                 action as granted the moment it sees it anywhere, with no way to represent a \
                 condition narrowing it, so a *new* Condition must be consciously reviewed and \
                 added to that list (does every derived op that needs this action still get it \
                 under the condition?), not silently pass.",
                sid, in_scope,
            );
        }

        actions.extend(in_scope);
    }

    actions
}

/// Direction: **per-file** — each policy individually must carry an unconditional `s3:ListBucket`
/// grant on the bare bucket resource (`${bucket_arn}`, not `${bucket_arn}/*`).
///
/// This is deliberately **not** folded into [`granted_actions`]'s union (PR #82 review, finding
/// 1). Direction B (`missing_grants`) answers "is `s3:ListBucket` granted *somewhere* across both
/// files" — and it stays satisfied by the control plane's `list_objects_v2`
/// (`discard_session`'s sweep) regardless of what the verifier's own file grants, because the
/// union model has no concept of *which* file a grant came from. That is exactly the shape of bug
/// this PR fixes: the verifier's policy had no `ListBucket` statement at all, and the existing
/// direction A/B/C test still reported 21 passed, 0 failed, because the control plane's grant
/// alone satisfied the union. A per-file check is the only way to catch a per-role regression in
/// this specific grant; see the module docs' residuals paragraph — per-role attribution is not
/// checked in general (that would need call-graph analysis), but this one grant is important and
/// cheap enough to pin directly rather than leave to the general residual.
fn has_unconditional_list_bucket_on_bucket(policy_json: &str) -> bool {
    let doc: serde_json::Value = serde_json::from_str(policy_json).expect("policy JSON parses");
    let statements = doc["Statement"].as_array().expect("Statement is an array");

    statements.iter().any(|statement| {
        if statement["Effect"].as_str() != Some("Allow") || statement.get("Condition").is_some() {
            return false;
        }

        let names_list_bucket = match &statement["Action"] {
            serde_json::Value::String(one) => one == "s3:ListBucket",
            serde_json::Value::Array(many) => {
                many.iter().any(|item| item.as_str() == Some("s3:ListBucket"))
            }
            _ => false,
        };

        names_list_bucket && statement["Resource"].as_str() == Some("${bucket_arn}")
    })
}

/// Direction A: every derived op must have an entry in `actions`.
fn unmapped_ops<'a>(derived: &'a BTreeSet<String>, actions: &BTreeMap<&str, &[&str]>) -> Vec<&'a String> {
    derived.iter().filter(|op| !actions.contains_key(op.as_str())).collect()
}

/// Direction B: every action a derived op requires must be granted somewhere.
fn missing_grants(
    derived: &BTreeSet<String>,
    actions: &BTreeMap<&str, &[&str]>,
    granted: &BTreeSet<String>,
) -> BTreeSet<String> {
    let required: BTreeSet<String> = derived
        .iter()
        .filter_map(|op| actions.get(op.as_str()))
        .flat_map(|required_actions| required_actions.iter().map(|a| a.to_string()))
        .collect();

    required.difference(granted).cloned().collect()
}

/// Direction C, JSON side: every granted action must be required by some derived op (kept from
/// v3 — see the module docs on why this was not dropped even though the design doc's "direction
/// C" text describes a different, code-side check).
fn dead_grants(
    derived: &BTreeSet<String>,
    actions: &BTreeMap<&str, &[&str]>,
    granted: &BTreeSet<String>,
) -> BTreeSet<String> {
    let required: BTreeSet<String> = derived
        .iter()
        .filter_map(|op| actions.get(op.as_str()))
        .flat_map(|required_actions| required_actions.iter().map(|a| a.to_string()))
        .collect();

    granted.difference(&required).cloned().collect()
}

// =========================================================================================
// The main test: both wrapper modules must pass every structural check, and the derived op
// set (their combined method-name surface) must reconcile with the policy JSON in both
// directions.
// =========================================================================================

#[test]
fn iam_policies_match_the_wrapper_op_surface() {
    let wrappers = [("S3Ops", S3_OPS_PATH), ("DynamoOps", DYNAMO_OPS_PATH)];
    let mut derived: BTreeSet<String> = BTreeSet::new();

    for (struct_name, path) in wrappers {
        let file = parse_source(&read_file(Path::new(path)));

        assert_eq!(
            checks::macro_count(&file),
            0,
            "{} ({}) contains a macro invocation — the wrapper module this whole design leans on \
             for parse totality must contain zero, since a macro's tokens are `syn`'s one blind \
             spot (escape 1, reproduced against v3).",
            struct_name,
            path,
        );

        let file_attr_violations = checks::file_attr_violations(&file);
        assert!(
            file_attr_violations.is_empty(),
            "{} ({}) fails the file-level attribute check:\n{}",
            struct_name,
            path,
            file_attr_violations.join("\n"),
        );

        let item_violations = checks::item_kind_violations(&file, struct_name);
        assert!(
            item_violations.is_empty(),
            "{} ({}) fails the item-kind allowlist:\n{}",
            struct_name,
            path,
            item_violations.join("\n"),
        );

        // The mechanism itself (PR #79 review, HIGH): the field must be private, and every op
        // method exactly `pub(crate)`. Nothing above this line checks either — item-kind and
        // attribute checks inspect item *kinds* and attributes, never `Visibility`.
        let field_violations = checks::field_privacy_violations(&file, struct_name);
        assert!(
            field_violations.is_empty(),
            "{} ({}) fails the field privacy check:\n{}",
            struct_name,
            path,
            field_violations.join("\n"),
        );

        let visibility_violations = checks::op_visibility_violations(&file, struct_name, "build");
        assert!(
            visibility_violations.is_empty(),
            "{} ({}) fails the op-method visibility check:\n{}",
            struct_name,
            path,
            visibility_violations.join("\n"),
        );

        let shape_violations = checks::op_method_shape_violations(&file, struct_name, "build");
        assert!(
            shape_violations.is_empty(),
            "{} ({}) fails the per-method shape check:\n{}",
            struct_name,
            path,
            shape_violations.join("\n"),
        );

        let call_violations = checks::constructor_call_violations(
            &file,
            struct_name,
            "build",
            CONSTRUCTOR_CALL_ALLOWLIST,
        );
        assert!(
            call_violations.is_empty(),
            "{} ({})'s constructor fails the config-builder call allowlist:\n{}",
            struct_name,
            path,
            call_violations.join("\n"),
        );

        derived.extend(checks::derived_ops(&file, struct_name, "build"));
    }

    let actions = op_actions();

    let unmapped = unmapped_ops(&derived, &actions);
    assert!(
        unmapped.is_empty(),
        "the wrapper modules expose op(s) {:?} with no entry in `op_actions` — a novel operation \
         must be mapped there, and granted in the policy JSON, before it can be added.",
        unmapped,
    );

    let granted: BTreeSet<String> = granted_actions(CONTROL_PLANE_POLICY, REVIEWED_CONDITIONED_SIDS)
        .union(&granted_actions(VERIFIER_POLICY, REVIEWED_CONDITIONED_SIDS))
        .cloned()
        .collect();

    let missing = missing_grants(&derived, &actions, &granted);
    assert!(
        missing.is_empty(),
        "the wrapper's derived operations require action(s) {:?} that neither policy JSON grants \
         — a missing grant.",
        missing,
    );

    let dead = dead_grants(&derived, &actions, &granted);
    assert!(
        dead.is_empty(),
        "the policy JSON grants action(s) {:?} that no wrapper op requires — a dead grant. \
         Remove it, or add the wrapper op that needs it.",
        dead,
    );
}

/// PR #82 review, finding 1: a **per-file** check that each policy individually grants an
/// unconditional `s3:ListBucket` on the bare bucket resource. See
/// [`has_unconditional_list_bucket_on_bucket`]'s docs for why this cannot be folded into the
/// union-based direction A/B/C test above — that test alone let the verifier's `ListBucket` grant
/// go missing entirely (this PR's own starting bug) and still reported every assertion green,
/// because the control plane's `list_objects_v2` grant satisfied the union regardless of which
/// file it lived in. Demonstrated both directions against the real files (not asserted): deleting
/// either policy's `ListBucketFor404Semantics` statement and running this test goes red; restoring
/// it goes green again — see the PR body/report for the four pasted runs.
#[test]
fn each_policy_grants_unconditional_list_bucket_on_the_bucket_per_file() {
    assert!(
        has_unconditional_list_bucket_on_bucket(CONTROL_PLANE_POLICY),
        "control-plane.policy.json must grant an unconditional s3:ListBucket on ${{bucket_arn}} \
         — HeadObject/GetObject 404-vs-403 semantics depend on it (docs/DEPLOYMENT.md, \"s3:\
         ListBucket on both roles is unconditional, and has to be\"). This is a per-file \
         assertion because the union-based direction B check cannot see a per-role gap in this \
         one grant (PR #82 review, finding 1)."
    );
    assert!(
        has_unconditional_list_bucket_on_bucket(VERIFIER_POLICY),
        "verifier.policy.json must grant an unconditional s3:ListBucket on ${{bucket_arn}} — \
         verify_and_promote's key_exists HeadObject on the canonical objects/{{hash}} key needs \
         it too (that key is absent by definition for a genuinely new object). This exact grant \
         was silently absent before this PR, and the union-based test alone never caught it."
    );
}

#[test]
fn client_types_and_bare_imports_appear_only_in_the_two_wrapper_modules() {
    let wrapper_paths: BTreeSet<PathBuf> =
        [PathBuf::from(S3_OPS_PATH), PathBuf::from(DYNAMO_OPS_PATH)].into_iter().collect();

    let mut leaked: BTreeMap<String, usize> = BTreeMap::new();
    let mut bare_imports: Vec<String> = Vec::new();

    for path in all_source_files() {
        let file = parse_source(&read_file(&path));
        let rel = path
            .strip_prefix(SRC_DIR)
            .expect("file under src/")
            .to_string_lossy()
            .replace('\\', "/"); // durable path keys use "/" — see the repo's own convention.

        if smuggling::has_bare_client_import(&file) {
            bare_imports.push(rel.clone());
        }

        if wrapper_paths.contains(&path) {
            continue; // the two sanctioned locations — expected to hold the raw type.
        }

        let count = smuggling::client_token_count(&file);
        if count > 0 {
            leaked.insert(rel, count);
        }
    }

    assert!(
        bare_imports.is_empty(),
        "found a bare or glob `use aws_sdk_s3::Client` / `use aws_sdk_dynamodb::Client` in {:?} — \
         every reference to the type must be spelled out in full so the token match below can see \
         it.",
        bare_imports,
    );

    assert!(
        leaked.is_empty(),
        "aws_sdk_s3::Client / aws_sdk_dynamodb::Client must appear in zero files other than \
         src/aws/s3_ops.rs and src/aws/dynamo_ops.rs. Found: {:?}",
        leaked,
    );
}

// =========================================================================================
// Committed escape probes (design doc point 8). Each asserts the checker function goes red
// against a synthetic snippet built to reproduce exactly the shape of hole it exists to catch
// — these are what promote the escape demonstrations from chat evidence into a regression a
// future change cannot quietly get past.
// =========================================================================================
#[cfg(test)]
mod escape_probes {
    use super::*;

    /// Probe 1 — a macro invocation inside a wrapper module must be red (escape 1: `syn` cannot
    /// see inside a macro's tokens, so parse totality is what stands in for it).
    #[test]
    fn probe_a_macro_in_wrapper_source_is_red() {
        let src = r#"
            pub struct S3Ops(aws_sdk_s3::Client);
            impl S3Ops {
                pub(crate) fn get_object(&self) -> GetObjectFluentBuilder {
                    println!("a macro call hiding right here");
                    self.0.get_object()
                }
            }
        "#;
        let file = parse_source(src);
        assert!(checks::macro_count(&file) > 0, "a macro invocation must be counted, not skipped");
    }

    /// Probe 2 — a `Deref` impl targeting the raw client must be red: it would silently reopen
    /// every operation the private field is supposed to gate, the single most dangerous item a
    /// default-red allowlist has to catch.
    #[test]
    fn probe_a_deref_impl_is_red() {
        let src = r#"
            pub struct S3Ops(aws_sdk_s3::Client);
            impl S3Ops {
                pub(crate) fn get_object(&self) -> GetObjectFluentBuilder { self.0.get_object() }
            }
            impl std::ops::Deref for S3Ops {
                type Target = aws_sdk_s3::Client;
                fn deref(&self) -> &Self::Target { &self.0 }
            }
        "#;
        let file = parse_source(src);
        let violations = checks::item_kind_violations(&file, "S3Ops");
        assert!(
            !violations.is_empty(),
            "a `Deref for S3Ops` impl must fail the item-kind allowlist, not pass as \"an impl of \
             the struct\""
        );
    }

    /// Probe 3 — an op method whose body calls a *differently-named* SDK operation must be red.
    /// This is the wrapper-side analogue of escape 2 (a call site that does not match the shape
    /// a scanner expects): the method's *name* says `get_object`, but its *body* actually grants
    /// `delete_object` — exactly the mismatch the per-method shape check exists to catch.
    #[test]
    fn probe_a_mismatched_delegation_body_is_red() {
        let src = r#"
            pub struct S3Ops(aws_sdk_s3::Client);
            impl S3Ops {
                pub(crate) fn get_object(&self) -> DeleteObjectFluentBuilder {
                    self.0.delete_object()
                }
            }
        "#;
        let file = parse_source(src);
        let violations = checks::op_method_shape_violations(&file, "S3Ops", "build");
        assert!(
            !violations.is_empty(),
            "a method named get_object delegating to .delete_object() must fail the shape check"
        );
    }

    /// Probe 4 — a raw `Client` token in a file other than the two wrapper modules must be red
    /// (escape 3, generalized: any file outside the two sanctioned ones naming the raw type).
    #[test]
    fn probe_a_raw_client_token_outside_the_wrapper_is_red() {
        let src = r#"
            pub struct Context {
                s3: aws_sdk_s3::Client,
            }
        "#;
        let file = parse_source(src);
        assert!(
            smuggling::client_token_count(&file) > 0,
            "a raw aws_sdk_s3::Client field must be counted as a leaked token"
        );
    }

    /// Probe 5 — a glob import of a sanctioned root must be red (v3's `UseTree::Glob(_) =>
    /// false` was the literal "unseen -> OK" bucket; this is the regression test for the fix).
    #[test]
    fn probe_a_glob_import_is_red() {
        let src = r#"
            use aws_sdk_s3::*;
        "#;
        let file = parse_source(src);
        assert!(
            smuggling::has_bare_client_import(&file),
            "a glob import of a sanctioned root must be flagged, not silently allowed"
        );
    }

    /// Probe 6 — an in-scope `Deny` statement must panic `granted_actions`, unconditionally.
    #[test]
    #[should_panic(expected = "Effect: Deny")]
    fn probe_a_deny_statement_panics() {
        granted_actions(
            r#"{"Version":"2012-10-17","Statement":[
                {"Sid":"Nope","Effect":"Deny","Action":"s3:DeleteObject","Resource":"*"}
            ]}"#,
            REVIEWED_CONDITIONED_SIDS,
        );
    }

    /// Probe 7 — an in-scope `Condition` on a `Sid` that is *not* in the allowlist passed in
    /// must panic — the default-red posture for a new, unreviewed narrowing. Passes
    /// `REVIEWED_CONDITIONED_SIDS` (empty today; see its own docs: `StagingSweep`'s entry was
    /// removed as a correction, not retired as an example still worth pointing at — the condition
    /// it allowlisted was a real bug, not a reviewed narrowing), so this probe's synthetic `Sid`
    /// would panic even if it happened to collide with a name that once appeared there. This
    /// probe alone does not exercise the allowlist's *other* half — see
    /// [`probe_a_reviewed_condition_statement_does_not_panic`] (PR #82 review, finding 4) for
    /// that.
    #[test]
    #[should_panic(expected = "reviewed-conditioned-Sids allowlist")]
    fn probe_an_unreviewed_condition_statement_panics() {
        granted_actions(
            r#"{"Version":"2012-10-17","Statement":[
                {"Sid":"SomeNewNarrowing","Effect":"Allow","Action":"s3:GetObject","Resource":"*",
                 "Condition":{"StringLike":{"s3:prefix":"objects/*"}}}
            ]}"#,
            REVIEWED_CONDITIONED_SIDS,
        );
    }

    /// Probe 7b (PR #82 review, finding 4) — a `Condition` on a `Sid` that **is** in the
    /// allowlist passed in must **not** panic. Before `granted_actions` took the allowlist as a
    /// parameter, this half of the mechanism was unfalsifiable: with the file-level
    /// `REVIEWED_CONDITIONED_SIDS` constant empty, the `&& !reviewed_conditioned_sids.
    /// contains(&sid)` term was true for every input any test in this file exercised, so deleting
    /// that term outright (collapsing the check to "any Condition panics, allowlist or not")
    /// left every test in this file green, including probe 7 above. Verified by literally
    /// deleting the term and re-running (see the PR report for the pasted red/green runs). This
    /// probe supplies a non-empty allowlist directly, independent of what
    /// `REVIEWED_CONDITIONED_SIDS` currently holds, so it stays meaningful even while that
    /// constant is empty.
    #[test]
    fn probe_a_reviewed_condition_statement_does_not_panic() {
        granted_actions(
            r#"{"Version":"2012-10-17","Statement":[
                {"Sid":"ReviewedSid","Effect":"Allow","Action":"s3:GetObject","Resource":"*",
                 "Condition":{"StringLike":{"s3:prefix":"objects/*"}}}
            ]}"#,
            &["ReviewedSid"],
        ); // must not panic
    }

    /// Probe 8 — an unmapped op (one `op_actions` has never heard of) must be red, direction A.
    #[test]
    fn probe_an_unmapped_op_is_red() {
        let derived: BTreeSet<String> = ["get_object".to_string(), "a_brand_new_operation".to_string()]
            .into_iter()
            .collect();
        let unmapped = unmapped_ops(&derived, &op_actions());
        assert!(
            unmapped.iter().any(|op| op.as_str() == "a_brand_new_operation"),
            "an operation with no `op_actions` entry must be reported as unmapped"
        );
    }

    /// Probe 9 — PR #79's HIGH: a `pub` field on the wrapper struct must be red. Reproduces the
    /// review's exact finding — `S3Ops(aws_sdk_s3::Client)` -> `S3Ops(pub aws_sdk_s3::Client)`
    /// left all ten v4 tests green, because none of them inspected `Visibility` at all.
    #[test]
    fn probe_a_public_field_is_red() {
        let src = r#"
            pub struct S3Ops(pub aws_sdk_s3::Client);
            impl S3Ops {
                pub(crate) fn get_object(&self) -> GetObjectFluentBuilder { self.0.get_object() }
            }
        "#;
        let file = parse_source(src);
        let violations = checks::field_privacy_violations(&file, "S3Ops");
        assert!(
            !violations.is_empty(),
            "a `pub` tuple field must fail the field privacy check — a public field lets \
             `ctx.s3.0.delete_bucket()` compile without ever naming `aws_sdk_s3::Client`, \
             reopening escape 3"
        );
    }

    /// Probe 10 — PR #79's HIGH, other half: an op method declared `pub` instead of
    /// `pub(crate)` must be red — directly callable from a bin or external test crate.
    #[test]
    fn probe_a_public_op_method_is_red() {
        let src = r#"
            pub struct S3Ops(aws_sdk_s3::Client);
            impl S3Ops {
                pub fn get_object(&self) -> GetObjectFluentBuilder { self.0.get_object() }
            }
        "#;
        let file = parse_source(src);
        let violations = checks::op_visibility_violations(&file, "S3Ops", "build");
        assert!(
            !violations.is_empty(),
            "a `pub` op method must fail the visibility check — `pub` is reachable from a \
             `[[bin]]` target or an external test crate, exactly escape 3"
        );
    }

    /// Probe 11 — a bare-private op method (no `pub(crate)` at all) must also be red: the
    /// *wrong* shape, not just an over-permissive one — it would break the sibling store module
    /// that is supposed to call it, so it should never pass as if it were the sanctioned form.
    #[test]
    fn probe_a_bare_private_op_method_is_red() {
        let src = r#"
            pub struct S3Ops(aws_sdk_s3::Client);
            impl S3Ops {
                fn get_object(&self) -> GetObjectFluentBuilder { self.0.get_object() }
            }
        "#;
        let file = parse_source(src);
        let violations = checks::op_visibility_violations(&file, "S3Ops", "build");
        assert!(!violations.is_empty(), "a bare-private op method must fail the visibility check");
    }

    /// Probe 12 — PR #79's MEDIUM 1: deleting `#![deny(dead_code)]` from a wrapper module must
    /// be red. Direction C (a dead op method) relies entirely on this attribute; without it,
    /// nothing catches an unused `pub(crate)` method.
    #[test]
    fn probe_missing_deny_dead_code_is_red() {
        let src = r#"
            pub struct S3Ops(aws_sdk_s3::Client);
            impl S3Ops {
                pub(crate) fn get_object(&self) -> GetObjectFluentBuilder { self.0.get_object() }
            }
        "#;
        let file = parse_source(src);
        let violations = checks::file_attr_violations(&file);
        assert!(
            !violations.is_empty(),
            "a wrapper module with no `#![deny(dead_code)]` at all must fail the file-attribute \
             check"
        );
    }

    /// Probe 13 — PR #79's MEDIUM 1, other half: `#![deny(dead_code)]` *plus*
    /// `#![allow(dead_code)]` right beside it must also be red — the `allow` would silently
    /// defeat the `deny` for every item in the file, which is exactly the surgical version of
    /// the same escape the per-method attribute check (probe on `item_kind_violations`) closes.
    #[test]
    fn probe_allow_dead_code_beside_deny_is_red() {
        let src = r#"
            #![deny(dead_code)]
            #![allow(dead_code)]
            pub struct S3Ops(aws_sdk_s3::Client);
            impl S3Ops {
                pub(crate) fn get_object(&self) -> GetObjectFluentBuilder { self.0.get_object() }
            }
        "#;
        let file = parse_source(src);
        let violations = checks::file_attr_violations(&file);
        assert!(
            !violations.is_empty(),
            "an `#![allow(dead_code)]` beside the `deny` must still fail the file-attribute check"
        );
    }

    /// Probe 14 — PR #79's MEDIUM 3, first form: `aws_sdk_s3::client::Client` (the crate's own
    /// `pub mod client` submodule path for the identical type, re-exported at the crate root as
    /// `aws_sdk_s3::Client`) must be counted as a leaked token just as the two-segment form is.
    #[test]
    fn probe_a_submodule_client_path_is_red() {
        let src = r#"
            pub struct Context {
                s3: aws_sdk_s3::client::Client,
            }
        "#;
        let file = parse_source(src);
        assert!(
            smuggling::client_token_count(&file) > 0,
            "aws_sdk_s3::client::Client (the submodule path) must be counted, not only the \
             crate-root re-export spelling"
        );
    }

    /// Probe 15 — PR #79's MEDIUM 3, second form: a crate-level rename alias
    /// (`use aws_sdk_s3 as sdk;`) followed by `sdk::Client::new(...)` must still be counted —
    /// the literal segment text `"aws_sdk_s3"` never appears in the call at all, so a check
    /// keyed on that text alone would miss it entirely.
    #[test]
    fn probe_a_crate_alias_client_path_is_red() {
        let src = r#"
            use aws_sdk_s3 as sdk;

            pub struct Context {
                s3: sdk::Client,
            }

            fn sneaky(ctx: &Context) {
                let _ = sdk::Client::new(&ctx.s3_config());
            }
        "#;
        let file = parse_source(src);
        assert!(
            smuggling::client_token_count(&file) > 0,
            "sdk::Client after `use aws_sdk_s3 as sdk;` must resolve through the alias and be \
             counted, not silently pass because the literal text \"aws_sdk_s3\" never appears"
        );
    }

    /// Probe 16 — PR #79's LOW 6: the attribute allowlist (inside `item_kind_violations`) must
    /// itself go red on a disallowed per-method attribute, not just on a struct/impl one.
    #[test]
    fn probe_a_disallowed_method_attribute_is_red() {
        let src = r#"
            pub struct S3Ops(aws_sdk_s3::Client);
            impl S3Ops {
                #[allow(dead_code)]
                pub(crate) fn get_object(&self) -> GetObjectFluentBuilder { self.0.get_object() }
            }
        "#;
        let file = parse_source(src);
        let violations = checks::item_kind_violations(&file, "S3Ops");
        assert!(
            !violations.is_empty(),
            "an `#[allow(dead_code)]` on one method must fail the item-kind allowlist's \
             attribute check, the surgical version of defeating direction C"
        );
    }

    /// Probe 17 — PR #79's LOW 6: the constructor-call allowlist must itself go red on a call
    /// whose name is not on it.
    #[test]
    fn probe_a_disallowed_constructor_call_is_red() {
        let src = r#"
            pub struct S3Ops(aws_sdk_s3::Client);
            impl S3Ops {
                pub(crate) fn build(shared: &aws_config::SdkConfig) -> S3Ops {
                    let client = aws_sdk_s3::Client::new(shared);
                    client.a_call_this_allowlist_has_never_seen();
                    S3Ops(client)
                }
                pub(crate) fn get_object(&self) -> GetObjectFluentBuilder { self.0.get_object() }
            }
        "#;
        let file = parse_source(src);
        let violations =
            checks::constructor_call_violations(&file, "S3Ops", "build", CONSTRUCTOR_CALL_ALLOWLIST);
        assert!(
            !violations.is_empty(),
            "a constructor call not on CONSTRUCTOR_CALL_ALLOWLIST must be reported"
        );
    }

    /// Probe 18 — PR #79's LOW 6: direction B (missing grant) must itself go red when a derived
    /// op needs an action neither policy grants.
    #[test]
    fn probe_a_missing_grant_is_red() {
        let derived: BTreeSet<String> = ["delete_object".to_string()].into_iter().collect();
        let actions = op_actions();
        let granted: BTreeSet<String> = BTreeSet::new(); // nothing granted at all.

        let missing = missing_grants(&derived, &actions, &granted);
        assert!(
            missing.contains("s3:DeleteObject"),
            "an action required by a derived op but granted nowhere must be reported missing"
        );
    }

    /// Probe 19 — PR #79's LOW 6: direction C, JSON side (dead grant) must itself go red when
    /// the policy grants an action no derived op requires.
    #[test]
    fn probe_a_dead_grant_is_red() {
        let derived: BTreeSet<String> = BTreeSet::new(); // no ops derived at all.
        let actions = op_actions();
        let granted: BTreeSet<String> = ["s3:DeleteObject".to_string()].into_iter().collect();

        let dead = dead_grants(&derived, &actions, &granted);
        assert!(
            dead.contains("s3:DeleteObject"),
            "a granted action no derived op requires must be reported as a dead grant"
        );
    }
}
