//! [`S3Ops`] — the sanctioned S3 operation surface, and nothing else.
//!
//! C11 (`tests/iam_conformance.rs`) is the fourth specification of the IAM conformance
//! guarantee. The first three were source scanners of increasing sophistication, and each
//! shipped a hole a scanner cannot see past: a macro body (`syn` never parses `ExprMacro`
//! tokens), a free function taking a raw client as a parameter (no `self.client` receiver to
//! match), and a `pub` client-returning constructor reachable from a bin (`src/bin/` was never
//! scanned). This module is why v4 does not need to enumerate escape routes: **rustc's
//! privacy checker**, not a parser, is what now makes them impossible.
//!
//! [`S3Ops`] wraps `aws_sdk_s3::Client` behind a **private** tuple field. The only way to reach
//! the client is this file's one inherent impl, and every non-constructor method on it is a
//! one-line delegation to the identically-named SDK method, returning the SDK's own fluent
//! builder (`self.0.<op>()`). Two things fall out of that for free:
//!
//! * **Existing call sites do not change.** `self.client.delete_object().bucket(..).key(..).send()`
//!   in `aws/s3.rs` still compiles once `client`'s type changes from `aws_sdk_s3::Client` to
//!   `S3Ops` — the method name, arity and return type are identical, only the receiver's type
//!   differs. Presigning is unaffected the same way: `.presigned(config)` is called on the
//!   fluent builder [`get_object`](S3Ops::get_object)/[`put_object`](S3Ops::put_object) hand
//!   back, not on `S3Ops` itself.
//! * **A fluent builder can only execute its own operation.** Handing one out — to a helper, a
//!   closure, anywhere — grants exactly the one op it was built for, never a route to any other
//!   SDK call. There is no aliasing-into-a-variable escape to close, because there is nothing
//!   dynamic to alias: the type system already knows the full set of ops before this module is
//!   ever parsed.
//!
//! The op methods are `pub(crate)`, not `pub`: a `[[bin]]` target (or an external integration
//! test) is a *different crate* from this library crate, so `pub(crate)` is invisible to it —
//! it can hold an `S3Ops` value (the struct itself is `pub`, so it is nameable as a field type),
//! but it cannot call `.delete_object()` on it directly. The only thing a bin can do with an
//! `S3Ops` is hand it to [`S3ObjectStore::new`](crate::aws::s3::S3ObjectStore::new), whose
//! reviewed key-layout logic (`aws/s3.rs`'s module docs, invariant 1) is what actually decides
//! which prefix an op ever touches. That is what closes the third demonstrated escape:
//! `build_clients` no longer returns a raw client a bin's `Context` can stash and call anything
//! on — see `aws/config.rs`.
//!
//! # What makes this module's own claim checkable, not just asserted
//!
//! Composing three separate, narrow properties — none of which alone would be enough — is the
//! actual guarantee, and each one has a committed test (`tests/iam_conformance.rs`) that goes
//! red if the property breaks:
//!
//! 1. **Parse totality.** `syn`'s only blind spot is macro-call tokens (`ExprMacro`), so the
//!    one guarantee-bearing property this file needs from parsing is that it contains *zero*
//!    macro invocations — not "none we noticed," an asserted zero. A macro here would be red,
//!    not silently skipped.
//! 2. **Item-kind allowlist, default-red.** This file may contain only `use` items, the
//!    `S3Ops` struct, and one inherent `impl S3Ops`. In particular, any *trait* impl is red —
//!    `impl Deref<Target = aws_sdk_s3::Client>` would silently undo the private-field
//!    encapsulation this whole module exists to provide, so it cannot be allowed to slip in
//!    even once. Any attribute beyond `#[derive(Clone, Debug)]` and doc comments is red too
//!    (closing the `#[allow(dead_code)]` route around direction C below).
//! 3. **Per-method shape.** Every non-constructor method's body must be exactly one expression:
//!    a method call on `self.0`, whose name matches the enclosing method's name, with zero
//!    arguments. [`build`](S3Ops::build) is the one constructor, exempted from that shape but
//!    checked against an explicit allowlist of the SDK config-builder calls it is allowed to
//!    make — a call not on that list is red, not silently accepted.
//!
//! The derived operation set is simply this impl's non-constructor method names — no AST walk
//! over `aws/s3.rs`/`aws/dynamo.rs` is needed anymore, because rustc's privacy already
//! guarantees nothing outside this file (and `dynamo_ops.rs`) can name an operation this impl
//! does not expose. `#[deny(dead_code)]` on the crate (see `lib.rs`) is what closes the
//! remaining direction: an op method nothing calls is a **build error**, not a passing test.
//!
//! # Residuals — not claimed to be closed here
//!
//! This module (plus its `dynamo_ops` sibling and the IAM-action map in
//! `tests/iam_conformance.rs`) closes the *completeness* question: every operation this crate
//! can possibly perform is exactly the set the scanner enumerates, and every one of those must
//! be mapped and granted. It does **not** check that `op_actions()`'s op → IAM-action mapping
//! is *itself correct* (a typo'd action there still passes); it does not attribute an action to
//! the specific role (control plane vs. verifier) that needs it, only the union; it says
//! nothing about `Resource`/`Condition` scoping correctness; and it is not a defense against an
//! adversarial committer willing to reach for `unsafe`/`transmute` (`#![forbid(unsafe_code)]` on
//! the crate is the relevant, separate guarantee for that). Those are, respectively, a review
//! and Layer 3 (a real-account deploy) concern, a call-graph-analysis concern, out of scope by
//! the crate's own KMS/CloudWatch precedent, and a different threat model than "a mistake",
//! not "a committer". This module's actual claim is the composition above — rustc privacy +
//! the zero-macro parse-totality assertion + the default-red allowlists, each backed by a
//! committed probe — not an unqualified "fails closed by construction".

// Direction C (a dead grant nothing calls) is rustc's job, not a scanner's: an op method below
// that nothing in the crate calls is a **build error** here, not a passing test. Scoped to this
// module (rather than the whole crate) so it applies exactly where `pub(crate)` op methods live.
#![deny(dead_code)]

use aws_sdk_s3::operation::copy_object::builders::CopyObjectFluentBuilder;
use aws_sdk_s3::operation::delete_object::builders::DeleteObjectFluentBuilder;
use aws_sdk_s3::operation::get_object::builders::GetObjectFluentBuilder;
use aws_sdk_s3::operation::head_object::builders::HeadObjectFluentBuilder;
use aws_sdk_s3::operation::list_objects_v2::builders::ListObjectsV2FluentBuilder;
use aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder;

use aws_smithy_http_client::tls;

use crate::aws::config::AwsConfig;

/// The sanctioned S3 capability: a private client, and one method per operation this crate is
/// allowed to perform. See the module docs for why the private field plus `pub(crate)` methods
/// is the mechanism, not merely documentation of intent.
#[derive(Clone, Debug)]
pub struct S3Ops(aws_sdk_s3::Client);

impl S3Ops {
    /// Build the S3 client from `config`, resolving credentials through the default provider
    /// chain and TLS through a **ring**-based rustls connector (see `aws/config.rs`'s module
    /// docs for why the connector is explicit rather than left to the SDK default).
    ///
    /// Exempt from the per-method shape check (it is the constructor), but every call it makes
    /// must be on the checker's config-builder allowlist — `tls_provider`, `build_https`,
    /// `defaults`, `latest`, `http_client`, `region`, `endpoint_url`, `load`, `from`,
    /// `force_path_style`, `build`, `from_conf`, `new` — so a call this test has never seen
    /// goes red until it is consciously added there.
    pub(crate) async fn build(config: &AwsConfig) -> Result<S3Ops, String> {
        let http_client = aws_smithy_http_client::Builder::new()
            .tls_provider(tls::Provider::Rustls(tls::rustls_provider::CryptoMode::Ring))
            .build_https();

        let mut loader =
            aws_config::defaults(aws_config::BehaviorVersion::latest()).http_client(http_client);

        if let Some(region) = &config.region {
            loader = loader.region(aws_sdk_s3::config::Region::new(region.clone()));
        }

        if let Some(endpoint) = &config.endpoint_url {
            loader = loader.endpoint_url(endpoint.clone());
        }

        let shared = loader.load().await;

        // LocalStack and MinIO serve one endpoint for every bucket, so the `bucket.host`
        // virtual addressing real S3 uses cannot resolve there; path-style (`host/bucket/key`)
        // is the form they understand. Real AWS keeps the default (virtual-host) addressing.
        let client = if config.endpoint_url.is_some() {
            let s3_config =
                aws_sdk_s3::config::Builder::from(&shared).force_path_style(true).build();
            aws_sdk_s3::Client::from_conf(s3_config)
        } else {
            aws_sdk_s3::Client::new(&shared)
        };

        Ok(S3Ops(client))
    }

    pub(crate) fn head_object(&self) -> HeadObjectFluentBuilder {
        self.0.head_object()
    }

    pub(crate) fn get_object(&self) -> GetObjectFluentBuilder {
        self.0.get_object()
    }

    pub(crate) fn put_object(&self) -> PutObjectFluentBuilder {
        self.0.put_object()
    }

    pub(crate) fn delete_object(&self) -> DeleteObjectFluentBuilder {
        self.0.delete_object()
    }

    pub(crate) fn copy_object(&self) -> CopyObjectFluentBuilder {
        self.0.copy_object()
    }

    pub(crate) fn list_objects_v2(&self) -> ListObjectsV2FluentBuilder {
        self.0.list_objects_v2()
    }
}
