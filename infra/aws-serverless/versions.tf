# Common-subset HCL: this file (and every other file in this module) must run unchanged under
# both OpenTofu and Terraform. See infra/aws-serverless/README.md and the design memo
# (forklift-planning/design-memos/2026-07-26-aws-serverless-terraform-reference.md, v4) for why:
# OpenTofu is the primary target (state encryption / early variable evaluation are OpenTofu-only
# and must never be used here), Terraform support is a first-class compatibility promise, not an
# afterthought.
#
# `>= 1.8.0` is the floor both toolchains satisfy today (OpenTofu 1.12.5, Terraform 1.13.5) and,
# concretely, the version each shipped `mock_provider` test support in — the whole Layer 1 test
# suite under tests/ depends on it. Do not lower this without checking that.
terraform {
  required_version = ">= 1.8.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}
