# Layer 1 (design memo §5): `tofu test` with `mock_provider` — free, credential-less, per-PR.
# Every C-claim below is genuinely bidirectional: revert the module line it pins and the assert
# reddens (demonstrated for C1/C2/C8/C9 in the slice-2 PR description). Runs entirely offline —
# no AWS credentials, no network calls beyond the one-time `tofu init` provider download.

mock_provider "aws" {}

# The mock provider fills every computed attribute with a random placeholder string by default,
# which is fine for most attributes here but not for the two role ARNs: `aws_lambda_function`'s
# `role` argument is validated client-side as ARN-shaped, and a bare random string fails that
# check before a mocked plan can even be produced. Override just these two to a plausible ARN;
# every other computed attribute (bucket ARNs, table ARN, function ARNs, log group ARNs, ...) is
# left to the provider's own placeholder, since nothing here depends on its exact shape.
override_resource {
  target = aws_iam_role.control_plane
  values = {
    arn = "arn:aws:iam::123456789012:role/forklift-control-plane-role"
  }
}

override_resource {
  target = aws_iam_role.verifier
  values = {
    arn = "arn:aws:iam::123456789012:role/forklift-verifier-role"
  }
}

# Same reasoning: aws_lambda_permission's source_arn is also ARN-validated client-side, and it
# is built from the bucket ARN and the API's execution ARN respectively.
override_resource {
  target = aws_s3_bucket.this
  values = {
    arn = "arn:aws:s3:::forklift-mock-bucket"
  }
}

override_resource {
  target = aws_apigatewayv2_api.this
  values = {
    execution_arn = "arn:aws:execute-api:us-east-1:123456789012:mockapiid"
  }
}

variables {
  control_plane_package = "./tests/fixtures/dummy.zip"
  verifier_package      = "./tests/fixtures/dummy.zip"
  auth_token            = "a-test-token-that-is-long-enough"
}

# ---------------------------------------------------------------------------------------------
# C1 — the bucket is never public: all four public-access-block flags asserted true.
# ---------------------------------------------------------------------------------------------

run "c1_bucket_never_public" {
  command = apply

  assert {
    condition     = aws_s3_bucket_public_access_block.this.block_public_acls == true
    error_message = "C1: block_public_acls must be true."
  }

  assert {
    condition     = aws_s3_bucket_public_access_block.this.block_public_policy == true
    error_message = "C1: block_public_policy must be true."
  }

  assert {
    condition     = aws_s3_bucket_public_access_block.this.ignore_public_acls == true
    error_message = "C1: ignore_public_acls must be true."
  }

  assert {
    condition     = aws_s3_bucket_public_access_block.this.restrict_public_buckets == true
    error_message = "C1: restrict_public_buckets must be true."
  }
}

# ---------------------------------------------------------------------------------------------
# C2 — staging/ lifecycle expiry exists, correct prefix, >= 1 day; plus a validation-failure
# test at 0 (the floor with no disable).
# ---------------------------------------------------------------------------------------------

run "c2_staging_lifecycle_rule" {
  command = apply

  assert {
    condition     = aws_s3_bucket_lifecycle_configuration.this.rule[0].id == "expire-staging"
    error_message = "C2: the first lifecycle rule must be the mandatory staging/ expiry."
  }

  assert {
    condition     = aws_s3_bucket_lifecycle_configuration.this.rule[0].filter[0].prefix == "staging/"
    error_message = "C2: the staging lifecycle rule must be scoped to the staging/ prefix."
  }

  assert {
    condition     = aws_s3_bucket_lifecycle_configuration.this.rule[0].expiration[0].days >= 1
    error_message = "C2: the staging lifecycle rule must expire at 1 day or later."
  }

  assert {
    condition     = aws_s3_bucket_lifecycle_configuration.this.rule[0].status == "Enabled"
    error_message = "C2: the staging lifecycle rule must be enabled."
  }
}

run "c2_staging_expiry_floor_rejects_zero" {
  command = plan

  variables {
    staging_expiry_days = 0
  }

  expect_failures = [
    var.staging_expiry_days,
  ]
}

# A throttle of 0 applies cleanly and then 429s every request on the API, which is
# indistinguishable from an outage at the call site. Pinned as a rejection rather than an assert:
# the guard is a variable validation, so the failure has to surface at plan time.
run "c2_throttle_floor_rejects_zero" {
  command = plan

  variables {
    throttling_rate_limit  = 0
    throttling_burst_limit = 0
  }

  expect_failures = [
    var.throttling_rate_limit,
    var.throttling_burst_limit,
  ]
}

# ---------------------------------------------------------------------------------------------
# C3 — the S3 event notification is ObjectCreated:* scoped to staging/ only.
# ---------------------------------------------------------------------------------------------

run "c3_notification_scoped_to_staging" {
  command = apply

  assert {
    condition     = tolist(aws_s3_bucket_notification.this.lambda_function)[0].events == toset(["s3:ObjectCreated:*"])
    error_message = "C3: the notification must fire on s3:ObjectCreated:* only."
  }

  assert {
    condition     = tolist(aws_s3_bucket_notification.this.lambda_function)[0].filter_prefix == "staging/"
    error_message = "C3: the notification must be scoped to the staging/ prefix."
  }

  assert {
    condition     = tolist(aws_s3_bucket_notification.this.lambda_function)[0].lambda_function_arn == aws_lambda_function.verifier.arn
    error_message = "C3: the notification must target the verifier Lambda."
  }
}

# ---------------------------------------------------------------------------------------------
# C4 — the verifier's env carries FORKLIFT_DYNAMODB_TABLE even though it never calls DynamoDB
# (the "operator quirk" both binaries share via config_from_env).
# ---------------------------------------------------------------------------------------------

run "c4_verifier_env_carries_table" {
  command = apply

  assert {
    condition     = aws_lambda_function.verifier.environment[0].variables["FORKLIFT_DYNAMODB_TABLE"] == aws_dynamodb_table.this.name
    error_message = "C4: the verifier's environment must carry FORKLIFT_DYNAMODB_TABLE, matching the deployed table."
  }

  assert {
    condition     = aws_lambda_function.verifier.environment[0].variables["FORKLIFT_S3_BUCKET"] == aws_s3_bucket.this.bucket
    error_message = "C4: the verifier's environment must also carry FORKLIFT_S3_BUCKET."
  }
}

# ---------------------------------------------------------------------------------------------
# C5 — open access is not constructible: FORKLIFT_OPEN_ACCESS is absent from both env maps, and
# an empty auth_token is a validation failure (never a valid empty token).
# ---------------------------------------------------------------------------------------------

run "c5_open_access_not_constructible" {
  command = apply

  assert {
    condition     = !contains(keys(aws_lambda_function.control_plane.environment[0].variables), "FORKLIFT_OPEN_ACCESS")
    error_message = "C5: no variable may ever map to FORKLIFT_OPEN_ACCESS on the control plane."
  }

  assert {
    condition     = !contains(keys(aws_lambda_function.verifier.environment[0].variables), "FORKLIFT_OPEN_ACCESS")
    error_message = "C5: no variable may ever map to FORKLIFT_OPEN_ACCESS on the verifier."
  }
}

run "c5_empty_auth_token_rejected" {
  command = plan

  variables {
    auth_token = ""
  }

  expect_failures = [
    var.auth_token,
  ]
}

# ---------------------------------------------------------------------------------------------
# C6 — the deployed IAM policies equal an independently rendered copy of the same crate JSON
# (pins Terraform<->JSON; JSON<->code is C11 in the crate, not this test).
# ---------------------------------------------------------------------------------------------

run "c6_deployed_policy_matches_crate_json" {
  command = apply

  assert {
    condition = aws_iam_role_policy.control_plane.policy == templatefile(
      "../../crates/forklift-aws-lambda/iam/control-plane.policy.json",
      {
        bucket_arn    = aws_s3_bucket.this.arn
        table_arn     = aws_dynamodb_table.this.arn
        log_group_arn = trimsuffix(aws_cloudwatch_log_group.control_plane.arn, ":*")
      },
    )
    error_message = "C6: the planned control-plane policy must equal an independent render of iam/control-plane.policy.json."
  }

  assert {
    condition = aws_iam_role_policy.verifier.policy == templatefile(
      "../../crates/forklift-aws-lambda/iam/verifier.policy.json",
      {
        bucket_arn    = aws_s3_bucket.this.arn
        log_group_arn = trimsuffix(aws_cloudwatch_log_group.verifier.arn, ":*")
      },
    )
    error_message = "C6: the planned verifier policy must equal an independent render of iam/verifier.policy.json."
  }
}

# ---------------------------------------------------------------------------------------------
# C7 — timeout/memory/architecture defaults match docs/DEPLOYMENT.md.
# ---------------------------------------------------------------------------------------------

run "c7_compute_defaults_match_the_doc" {
  command = apply

  assert {
    condition     = aws_lambda_function.control_plane.memory_size == 512
    error_message = "C7: control-plane default memory must be 512 MB per docs/DEPLOYMENT.md."
  }

  assert {
    condition     = aws_lambda_function.control_plane.timeout == 29
    error_message = "C7: control-plane default timeout must be 29s (the gateway ceiling)."
  }

  assert {
    condition     = aws_lambda_function.verifier.memory_size == 256
    error_message = "C7: verifier default memory must be 256 MB per docs/DEPLOYMENT.md."
  }

  assert {
    condition     = aws_lambda_function.verifier.timeout == 90
    error_message = "C7: verifier default timeout must be within the doc's 60-120s recommendation."
  }

  assert {
    condition     = tolist(aws_lambda_function.control_plane.architectures)[0] == "arm64"
    error_message = "C7: the default architecture must be arm64."
  }

  assert {
    condition     = tolist(aws_lambda_function.verifier.architectures)[0] == "arm64"
    error_message = "C7: the default architecture must be arm64."
  }
}

# ---------------------------------------------------------------------------------------------
# C8 — the safety posture holds under default vars: force_destroy == false, deletion_protection
# == true, enable_pitr == true. This exists because Layer 2 (LocalStack apply-and-destroy) must
# override every one of these, so without this pin the reference posture is exercised by
# nothing.
# ---------------------------------------------------------------------------------------------

run "c8_safety_posture_holds_by_default" {
  command = apply

  assert {
    condition     = aws_s3_bucket.this.force_destroy == false
    error_message = "C8: force_destroy must default to false."
  }

  assert {
    condition     = aws_dynamodb_table.this.deletion_protection_enabled == true
    error_message = "C8: deletion_protection must default to true."
  }

  assert {
    condition     = aws_dynamodb_table.this.point_in_time_recovery[0].enabled == true
    error_message = "C8: enable_pitr must default to true — it protects the only unrecoverable state."
  }
}

# ---------------------------------------------------------------------------------------------
# C9 — the gateway stage is $default. A named stage prepends /{stage} to every request path and
# breaks entrypoint::handle's absolute-path matching (see main.tf's comment on the resource).
# ---------------------------------------------------------------------------------------------

run "c9_stage_is_default" {
  command = apply

  assert {
    condition     = aws_apigatewayv2_stage.default[0].name == "$default"
    error_message = "C9: the API Gateway stage must be the literal $default stage."
  }

  assert {
    condition     = aws_apigatewayv2_stage.default[0].auto_deploy == true
    error_message = "C9: the $default stage must auto-deploy."
  }
}

# ---------------------------------------------------------------------------------------------
# C10 — the control-plane timeout cannot exceed the gateway's non-configurable 29s ceiling.
# ---------------------------------------------------------------------------------------------

run "c10_control_plane_timeout_above_ceiling_rejected" {
  command = plan

  variables {
    control_plane_timeout_s = 30
  }

  expect_failures = [
    var.control_plane_timeout_s,
  ]
}

# ---------------------------------------------------------------------------------------------
# C12/C13 — create_api (design memo §3.2, added 2026-07-26): an internal, test-harness-only
# toggle that skips the HTTP API entirely (Layer 2's LocalStack CI needs this, since LocalStack
# community does not implement API Gateway v2). Two properties are load-bearing together:
# create_api = false plans/applies cleanly when dev_endpoint_url is set (C12), and is rejected
# by validation when it is not (C13) — the footgun closes by construction, not by documentation.
# ---------------------------------------------------------------------------------------------

run "c12_create_api_false_skips_the_gateway" {
  command = apply

  variables {
    create_api       = false
    dev_endpoint_url = "http://localhost:4566"
  }

  assert {
    condition     = length(aws_apigatewayv2_api.this) == 0
    error_message = "C12: create_api = false must create zero aws_apigatewayv2_api resources."
  }

  assert {
    condition     = length(aws_apigatewayv2_integration.control_plane) == 0
    error_message = "C12: create_api = false must create zero aws_apigatewayv2_integration resources."
  }

  assert {
    condition     = length(aws_apigatewayv2_route.catch_all) == 0
    error_message = "C12: create_api = false must create zero aws_apigatewayv2_route resources."
  }

  assert {
    condition     = length(aws_apigatewayv2_stage.default) == 0
    error_message = "C12: create_api = false must create zero aws_apigatewayv2_stage resources."
  }

  assert {
    condition     = length(aws_lambda_permission.apigw_invoke) == 0
    error_message = "C12: create_api = false must create zero API Gateway Lambda invoke permissions."
  }

  assert {
    condition     = output.api_endpoint == null
    error_message = "C12: api_endpoint must output null (not empty string) when create_api = false."
  }

  assert {
    condition     = output.api_id == null
    error_message = "C12: api_id must output null (not empty string) when create_api = false."
  }
}

run "c13_create_api_false_without_dev_endpoint_is_rejected" {
  command = plan

  variables {
    create_api       = false
    dev_endpoint_url = null
  }

  expect_failures = [
    var.create_api,
  ]
}
