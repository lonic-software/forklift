# `infra/aws-serverless` — a reference OpenTofu/Terraform deployment

One module that mechanizes [`docs/DEPLOYMENT.md`](../../docs/DEPLOYMENT.md) — the AWS serverless
head (`crates/forklift-aws-lambda`) — as executable infrastructure. It is the *mechanized form*
of that document, not a second architecture: where the doc says "operator decision," the module
exposes a variable defaulted to the doc's own recommendation; where the doc says **must**, the
module enforces it with no off switch.

**License.** This directory is source-available under the Functional Source License 1.1, the same
terms as the crate it deploys — see [`../../LICENSING.md`](../../LICENSING.md). Contributions here
require a signed CLA, same as `crates/forklift-aws-lambda`.

**Not production-hardened, but not a toy either.** Every default is safe (private, encrypted,
fail-closed auth, rate-bounded, protected against accidental destroy). What production adds on
top — alarms/dashboards, a custom domain and WAF, a gateway authorizer, S3 versioning, CloudTrail
data events, token rotation, private networking, reserved concurrency, tagging governance — is
operational, not security-critical, and deliberately out of scope here.

## Tool: OpenTofu, and Terraform

This module is written in the **common HCL subset** both tools support — no OpenTofu-only
features (state encryption, early variable evaluation) and no Terraform-only ones. Both of the
following work unchanged:

```sh
tofu init && tofu apply
# or
terraform init && terraform apply
```

`required_version = ">= 1.8.0"` is the floor both toolchains satisfy today and, concretely, the
version each shipped full `mock_provider` test support in — this module's Layer 1 tests
(`tests/`) depend on that. The AWS provider is pinned with `~> 5.0`, resolved identically from
either tool's registry (`registry.opentofu.org` / `registry.terraform.io` mirror the same
upstream provider).

## What this module creates vs. accepts

**Creates, no BYO option:** the S3 bucket, the DynamoDB table, an HTTP API (API Gateway v2,
always deployed to the literal `$default` stage), both Lambda functions, both IAM execution
roles, both CloudWatch log groups, the S3 event notification wiring the bucket to the verifier,
and (optionally) a verifier dead-letter queue.

**Accepts:** the two Lambda deployment packages (this module never builds Rust — see
`docs/DEPLOYMENT.md` "Building the deployment artifacts"), the bearer auth token, an optional KMS
key ARN for SSE-KMS, and an optional IAM permissions boundary.

Why no BYO bucket/table/gateway: see the design memo
(`forklift-planning/design-memos/2026-07-26-aws-serverless-terraform-reference.md`, §3.1/§7) —
in short, the bucket's lifecycle rule and event notification are bucket-global singletons that a
foreign bucket's owner would fight; the table adds nothing over a name override; and a BYO
gateway would degrade several by-construction guarantees (route throttling, the `$default` pin,
the source-scoped invoke permission) to mere documentation. A custom domain — the usual reason to
want BYO-gateway — is already served: attach `aws_apigatewayv2_domain_name` plus an API mapping
to this module's `api_id` output from outside, with an **empty mapping key** (a non-empty one
reintroduces the same path-prefix footgun `$default` exists to avoid).

## Quickstart

See [`examples/complete/main.tf`](examples/complete/main.tf) — copy that directory, then:

```sh
cd examples/complete
tofu init      # or: terraform init
tofu apply \
  -var control_plane_package=/path/to/forklift-aws-control-plane.zip \
  -var verifier_package=/path/to/forklift-aws-verifier.zip \
  -var auth_token="$(openssl rand -base64 32)"
```

Build the two zips with `cargo lambda build --release --arm64` (see
`docs/DEPLOYMENT.md` "Building the deployment artifacts") — this module never shells out to
`cargo`.

## Footguns closed by construction

- **Deploying open access.** No variable maps to `FORKLIFT_OPEN_ACCESS`; `auth_token` is
  required and floor-validated. Open access is not constructible through this module.
- **Forgetting the staging lifecycle rule.** Always created, no disable, floor 1 day — nothing
  in `crates/forklift-aws-lambda` ever revisits an abandoned lift session, so without this an
  abandoned or paginating client leaks staged bytes forever.
- **The verifier missing its table env var.** Both Lambdas are wired from one config in this
  module, so `FORKLIFT_DYNAMODB_TABLE` cannot be forgotten — even though the verifier never
  issues a single DynamoDB operation (`config_from_env` requires it regardless).
- **A named API Gateway stage.** An HTTP API deployed to a *named* stage prepends `/{stage}` to
  every request path, and the router (`entrypoint::handle`) matches absolute paths — so every
  route would 404. This module hard-codes `$default` and exposes no stage variable at all.
- **REST-API binary mishandling.** Only an HTTP API (v2) is offered; a REST API needs explicit
  Binary Media Types configuration for the one raw-bytes response
  (`GET /v1/signatures/{hash}`), which an HTTP API handles transparently.
- **A control-plane timeout above the gateway's ceiling.** API Gateway enforces a hard,
  non-configurable 29-second integration timeout; `control_plane_timeout_s` is validated at plan
  time and cannot exceed it.

**Not closed by construction — still worth knowing:**

- **The auth token lives in Terraform/OpenTofu state and the Lambda console.** Anyone with
  `lambda:GetFunctionConfiguration` can read it; rotation is change-variable-and-apply. Use an
  encrypted state backend. This module deliberately does not add a Secrets Manager integration —
  the code reads an environment variable and nothing else, so that would imply security it
  cannot deliver.
- **Multi-warehouse mode is single-tenant on auth.** One `auth_token` rules every warehouse id
  when `warehouse_id` is left null.
- **Wrong-architecture package.** Not preventable in HCL; if you build an x86_64 zip and deploy
  with the arm64 default (or vice versa), every invocation fails with an exec-format error.
  `docs/DEPLOYMENT.md`'s verification checklist catches this on its first step.

## KMS

`kms_key_arn` is null by default (SSE-S3). When set, **both** Lambda execution roles are granted
`kms:Decrypt` and `kms:GenerateDataKey`, resource-scoped to that key — deliberately not a
narrower per-role split. The control plane needs both actions (it signs a presigned `PUT`, and
`presign_get`/`key_bytes` both issue a `get_object`); the verifier needs both too (`get_object` on
the staged key, `copy_object` promoting it). Omitting either role's grant fails in a different
shape: the control plane fails loudly and synchronously (every presigned `GET`/offloaded response
403s), the verifier fails silently and asynchronously (uploads succeed, promotion dies, objects
never become fetchable). See the design memo §3.1 for the full per-role/per-operation table.

## IAM policies come from the crate, not from this module

`iam/control-plane.policy.json` and `iam/verifier.policy.json`, in
`crates/forklift-aws-lambda/`, are derived from the crate's own SDK call sites (and enforced to
stay that way by `tests/iam_conformance.rs`'s C11 in the crate). This module renders them with
`templatefile()` and never hand-writes the S3/DynamoDB/Logs statements they already contain. Only
the deployment-conditional statements — KMS (above) and the DLQ send permission (below) — are
added here, since neither is the product of an unconditional SDK call.

## Variables and outputs

The full, semver-governed contract is in [`variables.tf`](variables.tf) and
[`outputs.tf`](outputs.tf) — every variable and output there is a permanent compatibility promise
once published; renames and removals are breaking changes. A summary:

**Required:** `control_plane_package`, `verifier_package`, `auth_token` (sensitive).

**Optional, most consequential first:** `kms_key_arn`, `warehouse_id`, `force_destroy`,
`deletion_protection`, `enable_pitr`, `staging_expiry_days`, `responses_expiry_days`,
`architecture`, memory/timeout/ephemeral-storage per function, `throttling_rate_limit`/
`throttling_burst_limit`, `create_verifier_dlq`, `log_retention_days`, `name_prefix`,
`bucket_name`, `table_name`, `default_pallet`, `permissions_boundary_arn`, `tags`,
`dev_endpoint_url` (LocalStack only).

**Outputs:** `api_endpoint`, `api_id`, bucket name/ARN, table name/ARN, both function names/ARNs,
both role ARNs (the sanctioned seam for attaching extra policies externally), the DLQ URL/ARN
(null when disabled), and both log group names.

## Testing

`tests/main.tftest.hcl` is this module's Layer 1: `tofu test` (or `terraform test`) with
`mock_provider "aws"` — free, credential-less, and runs in CI on every PR. It pins the C1-C10
claims from the design memo's §5, including two validation-failure cases
(`staging_expiry_days = 0`, an empty `auth_token`) and a gateway-ceiling rejection
(`control_plane_timeout_s` above 29). Each assertion is genuinely bidirectional — reverting the
module line it checks makes the assertion fail, not merely pass vacuously.

Layer 2 (LocalStack apply-and-exercise) and Layer 3 (a real-account scheduled deploy-and-verify)
are tracked separately; they pin the behavioral half of the notification/env claims and IAM
sufficiency respectively, which a plan-only test cannot reach.
