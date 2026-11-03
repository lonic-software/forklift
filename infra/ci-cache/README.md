# `infra/ci-cache` — the CI compile cache

The S3 bucket and IAM role behind the `test` job's compile cache in
[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml). That job runs
[sccache](https://github.com/mozilla/sccache) as `RUSTC_WRAPPER`, so a workspace crate that a push
did not change comes from the cache instead of being compiled again. `Swatinem/rust-cache` still
restores the dependencies; it leaves the workspace crates out, because cargo judges them by mtime
and a fresh checkout makes every file new. sccache keys on content, so it can cache them.

This is the project's own CI tooling, not part of Forklift, and is licensed like the rest of the
repository outside the server heads (see [`LICENSING.md`](../../LICENSING.md)).

## What it creates

- **A private bucket**, `forklift-sccache-<account id>`: public access blocked, objects owned by
  the bucket owner, SSE-S3 encryption, and a policy refusing non-TLS requests. A lifecycle rule
  deletes each object `expire_after_days` (default 14) after it was written, so the cache cannot
  grow without bound.
- **A role**, `forklift-ci-sccache`, with read and write on that bucket's objects and nothing
  else. GitHub Actions assumes it through OIDC, for runs of this repository's pull requests and of
  pushes to `main` only.

It reads, and does not own, the account's GitHub OIDC provider
(`token.actions.githubusercontent.com`), which other workflows use too. Destroying this root
leaves the provider in place.

## Who can write the cache

Every run allowed to assume the role can write entries that later runs, `main` included, will
use. That is the repository's pull requests from its own branches, so people with push access.
A pull request from a fork cannot: GitHub does not pass secrets to it, so CI skips the cache, and
it is given no token to assume the role with. The workflow syntax reference says pull requests
from forks have their write permissions downgraded to read, and an OIDC token needs
`id-token: write`. That holds unless an admin turns on *Send write tokens to workflows from pull
requests*, so leave that setting off.

## Set up

With credentials for the target account (the maintainer's is `--profile default`, in
`eu-central-1`):

```sh
cd infra/ci-cache
tofu init
tofu apply
```

Then give CI the two outputs as repository secrets:

```sh
gh secret set SCCACHE_BUCKET --body "$(tofu output -raw bucket)"
gh secret set SCCACHE_ROLE_ARN --body "$(tofu output -raw role_arn)"
```

While either secret is missing, CI builds without the cache, exactly as a fork does.

The maintainer's checkout keeps this root's state in the shared state bucket, through a gitignored
`backend_override.tf` beside these files, as `infra/aws-serverless/examples/staging` does:

```hcl
terraform {
  backend "s3" {
    bucket       = "forklift-tfstate-<account id>"
    key          = "ci-cache/terraform.tfstate"
    region       = "eu-central-1"
    use_lockfile = true
  }
}
```

## Reading the hit rate

Each job prints `sccache --show-stats` in its last step. A push that changes only tests or the CLI
should show hits for the crates it left alone. Linking is never cached, and neither is any crate
sccache judges uncacheable (the stats list those, with the reason).

## Tear down

```sh
gh secret delete SCCACHE_ROLE_ARN
gh secret delete SCCACHE_BUCKET
cd infra/ci-cache
tofu destroy
```

Delete the secrets first, so no run starts against a role that is going away. `tofu destroy`
refuses a bucket that still holds objects; empty it first with
`aws s3 rm s3://<bucket> --recursive`, or wait for the lifecycle rule to empty it.
