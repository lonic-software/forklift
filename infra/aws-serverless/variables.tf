# The module contract (§3 of the design memo). Variables and outputs are semver-governed —
# renames and removals are breaking changes — so this list is deliberately minimal. See
# docs/DEPLOYMENT.md for the deployment this module mechanizes.

# ---------------------------------------------------------------------------------------------
# Required.
# ---------------------------------------------------------------------------------------------

variable "control_plane_package" {
  description = <<-EOT
    Path to the control-plane Lambda deployment package (a zip with the `bootstrap` executable
    at its root — see docs/DEPLOYMENT.md "Building the deployment artifacts"). This module never
    builds Rust; it only deploys an artifact you already produced (e.g. via `cargo lambda build`).
  EOT
  type        = string
}

variable "verifier_package" {
  description = <<-EOT
    Path to the staging-verifier Lambda deployment package (same shape as
    `control_plane_package`).
  EOT
  type        = string
}

variable "auth_token" {
  description = <<-EOT
    The bearer token the control plane requires via `Authorization: Bearer <token>`
    (`FORKLIFT_TOKEN`). The code accepts any non-empty token; this module raises the floor to a
    minimum length so a trivial or accidentally-empty value cannot be deployed. There is no
    variable that maps to `FORKLIFT_OPEN_ACCESS` — deploying open access is not constructible
    through this module (see the README's "Footguns closed by construction").
  EOT
  type        = string
  sensitive   = true

  validation {
    condition     = length(trimspace(var.auth_token)) >= 16
    error_message = "auth_token must be at least 16 non-whitespace characters."
  }
}

# ---------------------------------------------------------------------------------------------
# Optional — naming.
# ---------------------------------------------------------------------------------------------

variable "name_prefix" {
  description = "Prefix applied to every resource name this module creates."
  type        = string
  default     = "forklift"

  validation {
    # PR #80 review, finding #9: name_prefix flows unvalidated into the S3 bucket name
    # ("{name_prefix}-{account_id}-{region}", locals.bucket_name in main.tf — a 63-char ceiling,
    # lowercase-alphanumeric-and-hyphen-only) and both IAM role names (64-char ceiling). Any of
    # uppercase, underscores, or an over-long prefix plans cleanly (name_prefix itself is just a
    # string) and only fails at apply, after the bucket and table already exist. The 20-char cap
    # here is deliberately conservative, not the exact S3 boundary: it leaves headroom for the
    # 12-digit account id, a hyphen on each side, and any real AWS region name (currently up to
    # ~14 chars) without this module having to track S3's full naming grammar (IP-address-like
    # names, consecutive periods, etc. are out of scope for this guard).
    condition     = can(regex("^[a-z0-9]([a-z0-9-]{0,18}[a-z0-9])?$", var.name_prefix))
    error_message = "name_prefix must be 1-20 characters, lowercase letters/digits/hyphens only, and must not start or end with a hyphen — it becomes part of the S3 bucket name and both IAM role names."
  }
}

variable "bucket_name" {
  description = "Override the generated S3 bucket name. Null picks a name derived from name_prefix, account id, and region."
  type        = string
  default     = null
}

variable "table_name" {
  description = "Override the generated DynamoDB table name. Null picks \"{name_prefix}-refs\"."
  type        = string
  default     = null
}

# ---------------------------------------------------------------------------------------------
# Optional — head configuration (mirrors config_from_env exactly, entrypoint.rs:84-112).
# ---------------------------------------------------------------------------------------------

variable "warehouse_id" {
  description = <<-EOT
    Null (the default) deploys in multi-warehouse mode (`FORKLIFT_WAREHOUSE_ID` unset) — routes
    live under `/warehouses/{id}/v1/...`. A non-null value deploys single-warehouse mode, serving
    fixed `/v1/...` routes. There is no invented default id: this mirrors `config_from_env`
    exactly. Note: the auth token is single-tenant regardless of this setting — one token rules
    every warehouse id in multi-warehouse mode (F8).
  EOT
  type        = string
  default     = null
}

variable "default_pallet" {
  description = "FORKLIFT_DEFAULT_PALLET override. Null lets the code fall back to its own default (`main`)."
  type        = string
  default     = null
}

# ---------------------------------------------------------------------------------------------
# Optional — compute.
# ---------------------------------------------------------------------------------------------

variable "architecture" {
  description = "Lambda instruction set architecture for both functions. arm64 (Graviton) is cheaper per GB-second and needs no native C toolchain for this crate's ring-based TLS stack (docs/DEPLOYMENT.md \"Architecture: arm64 or x86_64\")."
  type        = string
  default     = "arm64"

  validation {
    condition     = contains(["arm64", "x86_64"], var.architecture)
    error_message = "architecture must be either \"arm64\" or \"x86_64\"."
  }
}

variable "control_plane_memory_mb" {
  description = "Control-plane Lambda memory, in MB. Default 512 per docs/DEPLOYMENT.md \"Memory sizing\"; raise to 1024 if warehouses routinely stack near-maximal (64 MiB) trees or recipes."
  type        = number
  default     = 512

  validation {
    condition     = var.control_plane_memory_mb >= 128 && var.control_plane_memory_mb <= 10240
    error_message = "control_plane_memory_mb must be between 128 and 10240 (Lambda's own limits)."
  }
}

variable "verifier_memory_mb" {
  description = "Verifier Lambda memory, in MB. Default 256 per docs/DEPLOYMENT.md — its memory floor is small and flat regardless of blob size."
  type        = number
  default     = 256

  validation {
    condition     = var.verifier_memory_mb >= 128 && var.verifier_memory_mb <= 10240
    error_message = "verifier_memory_mb must be between 128 and 10240 (Lambda's own limits)."
  }
}

variable "control_plane_timeout_s" {
  description = <<-EOT
    Control-plane Lambda timeout, in seconds. API Gateway enforces a hard, non-configurable
    29-second integration timeout for a synchronous Lambda proxy integration (true of both REST
    and HTTP APIs — docs/DEPLOYMENT.md "Timeout guidance"); a value above that ceiling only burns
    compute after the client already has its 504, so this module rejects it at plan time (F9/C10).
  EOT
  type        = number
  default     = 29

  validation {
    condition     = var.control_plane_timeout_s >= 1 && var.control_plane_timeout_s <= 29
    error_message = "control_plane_timeout_s cannot exceed API Gateway's non-configurable 29-second integration timeout ceiling."
  }
}

variable "verifier_timeout_s" {
  description = "Verifier Lambda timeout, in seconds. Not behind API Gateway, so no 29s ceiling applies; docs/DEPLOYMENT.md recommends 60-120s."
  type        = number
  default     = 90

  validation {
    condition     = var.verifier_timeout_s >= 1 && var.verifier_timeout_s <= 900
    error_message = "verifier_timeout_s must be between 1 and 900 (Lambda's own ceiling)."
  }
}

variable "control_plane_ephemeral_storage_mb" {
  description = "Control-plane Lambda /tmp size, in MB. Lambda's own default (512) is likely fine to start; raise it if a warehouse has a long history and cold starts are common (docs/DEPLOYMENT.md \"Ephemeral storage\")."
  type        = number
  default     = 512

  validation {
    condition     = var.control_plane_ephemeral_storage_mb >= 512 && var.control_plane_ephemeral_storage_mb <= 10240
    error_message = "control_plane_ephemeral_storage_mb must be between 512 and 10240 (Lambda's own limits)."
  }
}

variable "verifier_ephemeral_storage_mb" {
  description = "Verifier Lambda /tmp size, in MB. The verifier only ever calls verify_and_promote, so it needs no scratch beyond Lambda's own default."
  type        = number
  default     = 512

  validation {
    condition     = var.verifier_ephemeral_storage_mb >= 512 && var.verifier_ephemeral_storage_mb <= 10240
    error_message = "verifier_ephemeral_storage_mb must be between 512 and 10240 (Lambda's own limits)."
  }
}

# ---------------------------------------------------------------------------------------------
# Optional — S3 lifecycle.
# ---------------------------------------------------------------------------------------------

variable "staging_expiry_days" {
  description = <<-EOT
    Lifecycle expiry, in days, for the `staging/` prefix. This rule is the one MUST in
    docs/DEPLOYMENT.md: nothing in the code ever revisits an abandoned lift session, so without
    it a crashed or paginating client leaks staged bytes forever. Always created; no disable;
    floor 1 day. Suggested value (and the default here) is 7 days.
  EOT
  type        = number
  default     = 7

  validation {
    condition     = var.staging_expiry_days >= 1
    error_message = "staging_expiry_days must be at least 1 — this rule cannot be disabled or set to zero."
  }
}

variable "responses_expiry_days" {
  description = <<-EOT
    Lifecycle expiry, in days, for the `responses/` prefix. Unlike staging_expiry_days this is a
    recommendation, not a MUST (docs/DEPLOYMENT.md "Recommended: expire the responses/ prefix") —
    every presigned GET to a `responses/` object is only valid for its own TTL regardless, since
    clients hash-verify every bundle record on import. Null omits the rule entirely.
  EOT
  type        = number
  default     = 1

  validation {
    condition     = var.responses_expiry_days == null || var.responses_expiry_days >= 1
    error_message = "responses_expiry_days must be at least 1, or null to omit the rule."
  }
}

# ---------------------------------------------------------------------------------------------
# Optional — logging.
# ---------------------------------------------------------------------------------------------

variable "log_retention_days" {
  description = "CloudWatch Logs retention, in days, for both functions' log groups."
  type        = number
  default     = 14

  validation {
    # PR #80 review, finding #8: CloudWatch Logs accepts only this enumerated set (verified
    # against the provider's own validator, terraform-provider-aws's
    # internal/service/logs/group.go, 2026-07-26) — anything else (e.g. 10 or 45) plans cleanly,
    # since this is just a number to OpenTofu/Terraform, and only fails at apply, after the
    # bucket and table already exist. 0 is CloudWatch's own sentinel for "never expire".
    condition = contains(
      [0, 1, 3, 5, 7, 14, 30, 60, 90, 120, 150, 180, 365, 400, 545, 731, 1096, 1827, 2192, 2557, 2922, 3288, 3653],
      var.log_retention_days
    )
    error_message = "log_retention_days must be one of CloudWatch Logs' enumerated retention values: 0, 1, 3, 5, 7, 14, 30, 60, 90, 120, 150, 180, 365, 400, 545, 731, 1096, 1827, 2192, 2557, 2922, 3288, 3653."
  }
}

# ---------------------------------------------------------------------------------------------
# Optional — encryption.
# ---------------------------------------------------------------------------------------------

variable "kms_key_arn" {
  description = <<-EOT
    ARN of a customer-managed KMS key for S3 object encryption. Null (the default) uses SSE-S3.
    When set, both Lambda execution roles are granted kms:Decrypt and kms:GenerateDataKey scoped
    to this key — deliberately not a narrower per-role set (§3 of the design memo: two earlier
    attempts at narrowing this were both wrong. The control plane needs Decrypt for presign_get
    and key_bytes and GenerateDataKey for the presigned PUT it signs; the verifier needs Decrypt
    to read a staged object and GenerateDataKey to copy-promote it — omitting either role's grant
    fails in a different way: the control plane fails loudly and synchronously, the verifier
    fails silently and asynchronously, so both must be checked, and both get the same two actions).
  EOT
  type        = string
  default     = null
}

# ---------------------------------------------------------------------------------------------
# Optional — data-destruction safety posture (§4.2: "secure by default" vs "cheap to tear down").
# ---------------------------------------------------------------------------------------------

variable "force_destroy" {
  description = "Allow `terraform/tofu destroy` to delete the S3 bucket even if it still holds objects. False by default: a reference stack holding a real warehouse must not evaporate on a reflexive destroy. Flip to true only for a disposable/staging stack."
  type        = bool
  default     = false
}

variable "deletion_protection" {
  description = "Enable DynamoDB table deletion protection. True by default, for the same reason as force_destroy."
  type        = bool
  default     = true
}

variable "enable_pitr" {
  description = <<-EOT
    Enable DynamoDB point-in-time recovery. True by default: objects in S3 are content-addressed
    and re-liftable, but the refs table is not, so this protects the only unrecoverable state.
    Note this is the one always-on default that is not free — PITR bills per GB-month of
    continuous backup — but it is justified by unrecoverability, not by being free.
  EOT
  type        = bool
  default     = true
}

# ---------------------------------------------------------------------------------------------
# Optional — throttling. Always applied; only the values are configurable (§4.2).
# ---------------------------------------------------------------------------------------------

variable "throttling_rate_limit" {
  description = "Default-route steady-state request rate limit (requests/second). There is no way to remove this throttle — a public endpoint without one lets drive-by traffic bill you for Lambda invocations even though every request 401s."
  type        = number
  default     = 50
}

variable "throttling_burst_limit" {
  description = "Default-route burst request limit."
  type        = number
  default     = 100
}

# ---------------------------------------------------------------------------------------------
# Optional — verifier dead-letter queue.
# ---------------------------------------------------------------------------------------------

variable "create_verifier_dlq" {
  description = "Create an SQS dead-letter queue for the verifier Lambda's asynchronous S3-event invocations, for visibility into anything that exhausts Lambda's own retry policy."
  type        = bool
  default     = false
}

# ---------------------------------------------------------------------------------------------
# Optional — org conventions.
# ---------------------------------------------------------------------------------------------

variable "permissions_boundary_arn" {
  description = "Permissions boundary ARN applied to both Lambda execution roles. Null (the default) applies none."
  type        = string
  default     = null
}

variable "tags" {
  description = "Tags applied to every resource this module creates that supports tagging."
  type        = map(string)
  default     = {}
}

# ---------------------------------------------------------------------------------------------
# Optional — local development.
# ---------------------------------------------------------------------------------------------

variable "dev_endpoint_url" {
  description = "FORKLIFT_AWS_ENDPOINT_URL override for both functions — LocalStack/MinIO only. Null in every real deployment."
  type        = string
  default     = null
}

variable "create_api" {
  description = <<-EOT
    INTERNAL — a test-harness variable, like dev_endpoint_url, and explicitly NOT part of this
    module's semver compatibility promise: it may change shape or vanish in a future release
    without that counting as a breaking change. Default true (an HTTP API is always created).

    Exists solely because LocalStack community edition does not implement API Gateway v2 —
    `apigatewayv2 create-api` answers "not yet implemented or pro feature" (design memo §8 risk
    1) — so Layer 2's LocalStack CI needs to deploy everything except the gateway to exercise
    the bucket -> S3 event -> verifier spine. Setting this false skips the HTTP API and every
    resource that hangs off it (the integration, the route, the $default stage, and the
    gateway's Lambda invoke permission); api_endpoint and api_id then output null rather than a
    dangling reference (outputs.tf).

    The validation below is load-bearing, not decorative: create_api = false is accepted only
    when dev_endpoint_url is also set. Without it, a headless stack (no API, so nothing can ever
    reach the control plane) would be constructible against real AWS — a footgun the rest of
    this module closes by construction, not by a README warning. See the design memo §3.2.
  EOT
  type        = bool
  default     = true

  validation {
    condition     = var.create_api || var.dev_endpoint_url != null
    error_message = "create_api = false is only permitted when dev_endpoint_url is also set (LocalStack-only test harness) — a headless stack must never be deployable against real AWS."
  }
}
