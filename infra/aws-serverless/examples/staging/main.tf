# The dogfooded staging deployment ("Layer 3" — see README.md's Testing section): a CMK-configured
# stack so the scheduled real-account verify workflow (../../verify.sh, run by
# .github/workflows/aws-serverless-verify.yml) continuously exercises the KMS grants on BOTH
# Lambda execution roles as a standing pin, rather than as a one-off manual check that
# could silently regress. See ../../verify.sh's header comment for exactly what that buys and
# what it still cannot prove.
#
# This is a DISPOSABLE/staging posture, not the module's reference posture — force_destroy and
# deletion_protection are flipped below from the module's safe defaults, specifically so this
# stack tears down in one command. Those defaults are pinned by C8 (tests/main.tftest.hcl) for a
# reason (§4.2): a reference deployment holding a real warehouse must not evaporate on a
# reflexive `destroy`. This directory's whole purpose is being repeatedly recreated and torn
# down, so the opposite property is what's wanted here — do not copy force_destroy/
# deletion_protection into a deployment that holds data you actually care about keeping.
#
# Quickstart:
#   cd infra/aws-serverless/examples/staging
#   tofu init      # or: terraform init
#   tofu apply -var-file=staging.tfvars.example \
#     -var control_plane_package=/path/to/forklift-aws-control-plane.zip \
#     -var verifier_package=/path/to/forklift-aws-verifier.zip \
#     -var auth_token="$(openssl rand -base64 32)" \
#     -var kms_key_arn="arn:aws:kms:us-east-1:123456789012:key/..."
# (auth_token and kms_key_arn are deliberately absent from staging.tfvars.example — the first is
# a secret, the second is account-specific; both are supplied at apply time, e.g. from the
# scheduled workflow's repository secrets. See that file's own header for why.)

terraform {
  # See ../../versions.tf's comment: >= 1.9.0 (not 1.8.0) is the real floor, and the fully
  # qualified provider source is what lets one committed lock file satisfy both toolchains'
  # `-lockfile=readonly` (PR #80 review, findings #4 and #10).
  required_version = ">= 1.9.0"

  required_providers {
    aws = {
      source  = "registry.opentofu.org/hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

provider "aws" {
  region = var.region
}

variable "region" {
  description = "AWS region to deploy into."
  type        = string
  default     = "us-east-1"
}

variable "control_plane_package" {
  description = "Path to the control-plane Lambda zip (see docs/DEPLOYMENT.md for how to build one)."
  type        = string
}

variable "verifier_package" {
  description = "Path to the verifier Lambda zip."
  type        = string
}

variable "auth_token" {
  description = "The bearer token the deployed head will require. Not in the committed tfvars file — it's a secret."
  type        = string
  sensitive   = true
}

variable "kms_key_arn" {
  description = <<-EOT
    ARN of the customer-managed KMS key backing this stack. Not in the committed tfvars file —
    it's account-specific (create the key once, then pass its ARN here at apply time). Setting
    this is the entire point of the staging stack: it converts the KMS
    interaction from a one-off manual check into something the scheduled verify.sh run exercises
    on every pass, on both Lambda execution roles.
  EOT
  type        = string
}

module "forklift" {
  source = "../.."

  control_plane_package = var.control_plane_package
  verifier_package      = var.verifier_package
  auth_token            = var.auth_token
  kms_key_arn           = var.kms_key_arn

  # Single-warehouse mode: verify.sh (and the scheduled workflow that runs it) can then talk to
  # fixed /v1/... routes without needing to know or pass a warehouse id. There is no "real"
  # identity behind this value; any fixed, non-empty string works equally well.
  warehouse_id = "staging"

  # --- Deviations from the module's safe-by-default posture, and why (§4.2, C8) ---

  # Module default: false. A reference deployment holding a real warehouse must not evaporate on
  # a reflexive `destroy`. This stack's whole purpose is being repeatedly torn down and recreated
  # by CI, so the opposite property — one-command teardown — is what's wanted here instead.
  force_destroy = true

  # Module default: true, same reasoning as force_destroy above. Flipped for the identical
  # one-command-teardown reason.
  deletion_protection = false

  # enable_pitr is deliberately left at the module's default (true) — NOT flipped alongside the
  # pair above. It protects against losing data mid-lifetime, which is a different concern from
  # wanting a fast teardown when the stack is deliberately destroyed; there's no reason a
  # disposable stack should also run without backups for as long as it's alive.

  tags = {
    purpose = "forklift-fork-60-layer-3-staging"
  }
}

output "api_endpoint" {
  value = module.forklift.api_endpoint
}

output "bucket_name" {
  value = module.forklift.bucket_name
}

output "table_name" {
  value = module.forklift.table_name
}
