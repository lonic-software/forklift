# Layer 2 (see README.md's Testing section): a real `tofu apply` of this module against
# LocalStack, exercising the behavioural half of C3/C4 that a plan-only Layer 1 assert cannot
# reach — does the S3 event actually deliver, and does the verifier actually cold-start and
# promote?
#
# This is a separate root configuration, not `examples/complete`, because it wires a provider
# pointed entirely at LocalStack (fake credentials, every service endpoint overridden) and sets
# `create_api = false` — the internal test-harness toggle (variables.tf) that exists because
# LocalStack community does not implement API Gateway v2. Nothing here is part of the module's
# public contract; this directory documents how to *drive* the module, not how to consume it.

terraform {
  # >= 1.9.0, not 1.8.0 (PR #80 review) — matches the module's own floor (../../versions.tf);
  # see that file's comment for why 1.8.x cannot even parse this module's variables.tf.
  required_version = ">= 1.9.0"

  required_providers {
    aws = {
      # Fully qualified host, matching ../../versions.tf's fix for the same reason: one lock
      # file readable by both toolchains under `-lockfile=readonly` (PR #80 review, finding #10).
      source = "registry.opentofu.org/hashicorp/aws"
      # Layer 2's own constraint, tighter than the module's `~> 5.0` (versions.tf): AWS
      # provider >= 5.70.0 added an eventual-consistency waiter to
      # aws_s3_bucket_lifecycle_configuration that unconditionally compares
      # transition_default_minimum_object_size against the x-amz-transition-default-
      # minimum-object-size response header. LocalStack (and every other non-AWS S3-compatible
      # endpoint) never sends that header, so the provider reads it as empty, compares against
      # its own default ("all_storage_classes_128K"), and polls for 3 minutes before failing —
      # every apply against LocalStack, never against real AWS
      # (github.com/hashicorp/terraform-provider-aws#49019, confirmed no fix or workaround as of
      # 2026-07-26 beyond staying below the version that introduced the waiter). This has no
      # bearing on the module itself, which real deployments run against real S3.
      version = ">= 5.0, < 5.70.0"
    }
  }
}

provider "aws" {
  region     = "us-east-1"
  access_key = "test"
  secret_key = "test"

  # LocalStack accepts any credentials; these three skips avoid calls the fake credentials
  # would otherwise fail (STS GetCallerIdentity, the EC2 metadata probe).
  skip_credentials_validation = true
  skip_metadata_api_check     = true
  skip_requesting_account_id  = true
  s3_use_path_style           = true

  endpoints {
    s3       = var.localstack_endpoint
    dynamodb = var.localstack_endpoint
    lambda   = var.localstack_endpoint
    iam      = var.localstack_endpoint
    sts      = var.localstack_endpoint
    logs     = var.localstack_endpoint
    sqs      = var.localstack_endpoint
  }
}

variable "localstack_endpoint" {
  description = "The LocalStack endpoint the *host* talks to — the AWS provider (running tofu/terraform) and any exercise script (aws-cli, staging the test object, polling for promotion)."
  type        = string
  default     = "http://localhost:4566"
}

variable "lambda_endpoint_url" {
  description = <<-EOT
    The LocalStack endpoint the *deployed Lambda functions themselves* reach it at — this
    becomes FORKLIFT_AWS_ENDPOINT_URL via the module's dev_endpoint_url, and it is deliberately
    a DIFFERENT address than localstack_endpoint. `localhost:4566` is only meaningful from the
    host (or from LocalStack's own container); a Lambda invocation runs in its own nested Docker
    container (LocalStack's Lambda executor spins one up per function via the mounted Docker
    socket, per-invocation), where `localhost` refers to that nested container itself and
    LocalStack is unreachable there — confirmed by reproducing it directly: the verifier's
    `head_object` failed with a `Connection refused` dispatch error until this was fixed.

    The default here, `http://localstack:4566`, is a plain container hostname, resolved by
    Docker's embedded DNS for any two containers sharing a **user-defined** network — this is
    core Docker behaviour, identical on Docker Desktop and on a bare Linux dockerd, not a
    convenience layer either one adds. It requires exactly two things at the LocalStack side,
    both set by whatever starts LocalStack (locally: `docker network create` + `docker run
    --network`; in CI: the equivalent `docker` steps in `infra-localstack.yml` — deliberately
    NOT a `services:` block, see that workflow's header comment for why):

      1. LocalStack's own container is named `localstack` and attached to a user-defined
         network (not the default `bridge`, which does no name-based DNS at all).
      2. `LAMBDA_DOCKER_NETWORK` is set to that same network's name, so LocalStack attaches
         every Lambda execution container it spins up to it too — making `localstack` resolve
         from inside them.

    An EARLIER version of this pointed here at `http://host.docker.internal:4566` — Docker
    Desktop's magic host-reachable hostname. That works on a developer's Mac (verified) but
    is a Desktop-only convenience: plain Linux dockerd (every GitHub Actions runner) does not
    resolve it inside a container without an explicit `--add-host=host.docker.internal:
    host-gateway`, which is exactly the kind of platform-specific special-casing the network
    hostname above avoids entirely. This surfaced as a real CI failure (the apply and the
    staging PUT succeeded; the promotion poll then timed out for 80s, because the verifier's
    nested container could not resolve `host.docker.internal` on the Linux runner) — do not
    reintroduce it. Local and CI now drive LocalStack the identical way for exactly this reason:
    a mechanism that only one of them exercises is a mechanism only one of them proves.
  EOT
  type        = string
  default     = "http://localstack:4566"
}

variable "control_plane_package" {
  description = "Path to the control-plane Lambda zip, built for x86_64 (see the module README: Layer 2 must override the arm64 default to match)."
  type        = string
}

variable "verifier_package" {
  description = "Path to the verifier Lambda zip, built for x86_64."
  type        = string
}

variable "auth_token" {
  description = "The bearer token for this run (unused by Layer 2's exercise, which never calls the control plane — required by the module regardless)."
  type        = string
  sensitive   = true
}

variable "name_prefix" {
  description = "Resource name prefix — unique per CI run so concurrent/repeat runs never collide."
  type        = string
  default     = "forklift-l2"
}

module "forklift" {
  source = "../.."

  control_plane_package = var.control_plane_package
  verifier_package      = var.verifier_package
  auth_token            = var.auth_token
  name_prefix           = var.name_prefix

  # The x86_64 override ("Layer 2 must also set architecture = x86_64 explicitly"):
  # cargo-lambda's cross-compiled zips here are x86_64, but the module defaults to arm64 — an
  # unoverridden apply would create arm64 functions loaded with x86_64 code, and every
  # invocation would fail with an exec-format error for a reason unrelated to the module itself.
  architecture = "x86_64"

  # The two properties create_api's validation exists to keep coupled: create_api = false is
  # only reachable because dev_endpoint_url is set right here. dev_endpoint_url is the address
  # the *Lambda functions* use at runtime, not the Terraform-provider/exercise-script address —
  # see lambda_endpoint_url's own comment for why those two must differ.
  create_api       = false
  dev_endpoint_url = var.lambda_endpoint_url

  # C8 exists because Layer 2 must override both of these — the reference posture (safe
  # defaults) is exercised by nothing unless something is proven to need overriding it for a
  # disposable stack. force_destroy lets the bucket destroy despite holding objects;
  # deletion_protection lets the table destroy at all.
  force_destroy       = true
  deletion_protection = false
}

output "bucket_name" {
  value = module.forklift.bucket_name
}

output "table_name" {
  value = module.forklift.table_name
}

output "verifier_function_name" {
  value = module.forklift.verifier_function_name
}

output "control_plane_function_name" {
  value = module.forklift.control_plane_function_name
}

output "api_endpoint" {
  value = module.forklift.api_endpoint
}

output "api_id" {
  value = module.forklift.api_id
}
