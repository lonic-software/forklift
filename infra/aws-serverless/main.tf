# infra/aws-serverless — the mechanized form of docs/DEPLOYMENT.md. See the module's README.md
# for the quickstart and infra/aws-serverless/../../forklift-planning design memo
# (2026-07-26-aws-serverless-terraform-reference.md) for the contract this file implements.

data "aws_caller_identity" "current" {}

locals {
  bucket_name = coalesce(var.bucket_name, "${var.name_prefix}-${data.aws_caller_identity.current.account_id}-${data.aws_region.current.name}")
  table_name  = coalesce(var.table_name, "${var.name_prefix}-refs")

  control_plane_function_name = "${var.name_prefix}-control-plane"
  verifier_function_name      = "${var.name_prefix}-verifier"

  # entrypoint::config_from_env (entrypoint.rs:84-112) is the whole configuration surface both
  # binaries share. `aws_lambda_function`'s `environment.variables` is `map(string)` and rejects
  # a null element at plan time, so every optional variable is merged in only when non-null
  # rather than ever assigned null — this is what C4/C5's env-map asserts depend on.
  control_plane_env = merge(
    {
      FORKLIFT_S3_BUCKET      = aws_s3_bucket.this.bucket
      FORKLIFT_DYNAMODB_TABLE = aws_dynamodb_table.this.name
      FORKLIFT_TOKEN          = var.auth_token
    },
    var.warehouse_id != null ? { FORKLIFT_WAREHOUSE_ID = var.warehouse_id } : {},
    var.default_pallet != null ? { FORKLIFT_DEFAULT_PALLET = var.default_pallet } : {},
    var.dev_endpoint_url != null ? { FORKLIFT_AWS_ENDPOINT_URL = var.dev_endpoint_url } : {},
  )

  # The verifier calls the same config_from_env, so FORKLIFT_DYNAMODB_TABLE must be set even
  # though verify_and_promote never issues a single DynamoDB operation (the "operator quirk",
  # docs/DEPLOYMENT.md, pinned by C4) — a module that wires both functions from one config
  # cannot forget this. No FORKLIFT_TOKEN (the verifier is never invoked by a bearer-carrying
  # client — it is triggered by an S3 event, entrypoint.rs), no FORKLIFT_WAREHOUSE_ID (promotion
  # is content-addressed, not warehouse-scoped), no FORKLIFT_DEFAULT_PALLET (control-plane only).
  verifier_env = merge(
    {
      FORKLIFT_S3_BUCKET      = aws_s3_bucket.this.bucket
      FORKLIFT_DYNAMODB_TABLE = aws_dynamodb_table.this.name
    },
    var.dev_endpoint_url != null ? { FORKLIFT_AWS_ENDPOINT_URL = var.dev_endpoint_url } : {},
  )
}

data "aws_region" "current" {}

# ---------------------------------------------------------------------------------------------
# S3 bucket. Created, never accepted (BYO) — aws_s3_bucket_notification and the lifecycle
# configuration are both bucket-global singletons, so managing them on a foreign bucket would
# clobber whatever the owner already has there (§3.1, F6).
# ---------------------------------------------------------------------------------------------

resource "aws_s3_bucket" "this" {
  bucket        = local.bucket_name
  force_destroy = var.force_destroy
  tags          = var.tags
}

# Always on, no variable — all four flags (§4.2). Presigned URLs are authenticated requests and
# unaffected by any of these.
resource "aws_s3_bucket_public_access_block" "this" {
  bucket = aws_s3_bucket.this.id

  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_server_side_encryption_configuration" "this" {
  bucket = aws_s3_bucket.this.id

  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm     = var.kms_key_arn != null ? "aws:kms" : "AES256"
      kms_master_key_id = var.kms_key_arn
    }
    bucket_key_enabled = var.kms_key_arn != null
  }
}

# A literal jsonencode() rather than an aws_iam_policy_document data source: the latter's
# rendered `.json` is itself a provider-computed value, which under `mock_provider` (tests/) is
# a placeholder string rather than real JSON — a resource that locally JSON-validates its policy
# argument (as this one does) would fail to plan at all. A config-time constant sidesteps that
# entirely and is exactly as readable here.
resource "aws_s3_bucket_policy" "this" {
  bucket = aws_s3_bucket.this.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid       = "DenyInsecureTransport"
        Effect    = "Deny"
        Principal = "*"
        Action    = "s3:*"
        Resource = [
          aws_s3_bucket.this.arn,
          "${aws_s3_bucket.this.arn}/*",
        ]
        Condition = {
          Bool = {
            "aws:SecureTransport" = "false"
          }
        }
      },
    ]
  })
}

# The staging/ rule is a MUST (docs/DEPLOYMENT.md "Required: expire the staging/ prefix"):
# nothing in the code ever revisits an abandoned lift session, so a crashed or paginating client
# leaks staged bytes forever without it. Always created, no disable, floor 1 day — pinned by C2.
# The responses/ rule is a recommendation, not a MUST, so it is the one dynamic block here.
resource "aws_s3_bucket_lifecycle_configuration" "this" {
  bucket = aws_s3_bucket.this.id

  rule {
    id     = "expire-staging"
    status = "Enabled"

    filter {
      prefix = "staging/"
    }

    expiration {
      days = var.staging_expiry_days
    }
  }

  dynamic "rule" {
    for_each = var.responses_expiry_days != null ? [var.responses_expiry_days] : []

    content {
      id     = "expire-responses"
      status = "Enabled"

      filter {
        prefix = "responses/"
      }

      expiration {
        days = rule.value
      }
    }
  }
}

# s3:ObjectCreated:* on staging/ only, targeting the verifier — anything else is wasted
# invocations, not a correctness bug (docs/DEPLOYMENT.md "Event notification wiring"), but the
# module still scopes it exactly as documented. Pinned (statically) by C3.
resource "aws_s3_bucket_notification" "this" {
  bucket = aws_s3_bucket.this.id

  lambda_function {
    lambda_function_arn = aws_lambda_function.verifier.arn
    events              = ["s3:ObjectCreated:*"]
    filter_prefix       = "staging/"
  }

  depends_on = [aws_lambda_permission.s3_invoke_verifier]
}

resource "aws_lambda_permission" "s3_invoke_verifier" {
  statement_id  = "AllowS3Invoke"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.verifier.function_name
  principal     = "s3.amazonaws.com"
  source_arn    = aws_s3_bucket.this.arn
}

# ---------------------------------------------------------------------------------------------
# DynamoDB table. Created, never accepted (BYO) — the schema is fixed and owned by the head;
# BYO adds only naming, which table_name already covers.
# ---------------------------------------------------------------------------------------------

resource "aws_dynamodb_table" "this" {
  name         = local.table_name
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "wh"
  range_key    = "entity"

  attribute {
    name = "wh"
    type = "S"
  }

  attribute {
    name = "entity"
    type = "S"
  }

  point_in_time_recovery {
    enabled = var.enable_pitr
  }

  deletion_protection_enabled = var.deletion_protection

  tags = var.tags
}

# ---------------------------------------------------------------------------------------------
# CloudWatch log groups. Created explicitly (rather than relying on Lambda's auto-created,
# never-expiring group) so retention is actually bounded, and so their ARNs are known before the
# functions exist for the IAM policy templates below.
# ---------------------------------------------------------------------------------------------

resource "aws_cloudwatch_log_group" "control_plane" {
  name              = "/aws/lambda/${local.control_plane_function_name}"
  retention_in_days = var.log_retention_days
  tags              = var.tags
}

resource "aws_cloudwatch_log_group" "verifier" {
  name              = "/aws/lambda/${local.verifier_function_name}"
  retention_in_days = var.log_retention_days
  tags              = var.tags
}

locals {
  # aws_cloudwatch_log_group.arn has historically included a trailing ":*" straight from the
  # CloudWatch Logs API — trimsuffix makes this correct either way, since the policy JSON below
  # appends its own ":*" and a doubled suffix would otherwise result.
  control_plane_log_group_arn = trimsuffix(aws_cloudwatch_log_group.control_plane.arn, ":*")
  verifier_log_group_arn      = trimsuffix(aws_cloudwatch_log_group.verifier.arn, ":*")
}

# ---------------------------------------------------------------------------------------------
# IAM roles. Created, with an optional permissions boundary — BYO roles risk deploying without
# the derived policy (§3.1). Policies are rendered from the crate's JSON (never hand-written
# HCL) via templatefile(); CloudWatch Logs grants already live in that JSON (added in slice 1).
# KMS and the DLQ send permission are deployment-conditional and are added here, not in the JSON
# (§4.1's line: only what is *derived from an SDK call* belongs in the crate).
# ---------------------------------------------------------------------------------------------

# A literal jsonencode() constant, same reasoning as aws_s3_bucket_policy.this above — this
# never varies per deployment, so there is no reason to route it through a data source (and
# under mock_provider, a data source's computed .json output is a placeholder, not real JSON).
locals {
  lambda_assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Action = "sts:AssumeRole"
        Principal = {
          Service = "lambda.amazonaws.com"
        }
      },
    ]
  })
}

resource "aws_iam_role" "control_plane" {
  name                 = "${local.control_plane_function_name}-role"
  assume_role_policy   = local.lambda_assume_role_policy
  permissions_boundary = var.permissions_boundary_arn
  tags                 = var.tags
}

resource "aws_iam_role" "verifier" {
  name                 = "${local.verifier_function_name}-role"
  assume_role_policy   = local.lambda_assume_role_policy
  permissions_boundary = var.permissions_boundary_arn
  tags                 = var.tags
}

resource "aws_iam_role_policy" "control_plane" {
  name = "${local.control_plane_function_name}-policy"
  role = aws_iam_role.control_plane.id

  policy = templatefile("${path.module}/../../crates/forklift-aws-lambda/iam/control-plane.policy.json", {
    bucket_arn    = aws_s3_bucket.this.arn
    table_arn     = aws_dynamodb_table.this.arn
    log_group_arn = local.control_plane_log_group_arn
  })
}

resource "aws_iam_role_policy" "verifier" {
  name = "${local.verifier_function_name}-policy"
  role = aws_iam_role.verifier.id

  policy = templatefile("${path.module}/../../crates/forklift-aws-lambda/iam/verifier.policy.json", {
    bucket_arn    = aws_s3_bucket.this.arn
    log_group_arn = local.verifier_log_group_arn
  })
}

# KMS, when configured: {kms:Decrypt, kms:GenerateDataKey} on *both* roles, resource-scoped to
# the key. Deliberately not a narrower per-role set — two earlier attempts at deriving one were
# both wrong (§3.1 of the design memo). The control plane needs both actions (a presigned PUT it
# signs, presign_get's get_object, key_bytes' direct get_object); the verifier needs both too
# (get_object on the staged key, copy_object promoting it). Omitting either role fails in a
# different shape — the control plane loudly and synchronously, the verifier silently and
# asynchronously — which is exactly why this is not asserted per-role.
resource "aws_iam_role_policy" "kms" {
  for_each = var.kms_key_arn != null ? toset(["control_plane", "verifier"]) : toset([])

  name = "${var.name_prefix}-${each.key}-kms"
  role = each.key == "control_plane" ? aws_iam_role.control_plane.id : aws_iam_role.verifier.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid      = "CmkForS3"
        Effect   = "Allow"
        Action   = ["kms:Decrypt", "kms:GenerateDataKey"]
        Resource = var.kms_key_arn
      },
    ]
  })
}

resource "aws_sqs_queue" "verifier_dlq" {
  count = var.create_verifier_dlq ? 1 : 0

  name = "${local.verifier_function_name}-dlq"
  tags = var.tags
}

resource "aws_iam_role_policy" "verifier_dlq" {
  count = var.create_verifier_dlq ? 1 : 0

  name = "${local.verifier_function_name}-dlq-policy"
  role = aws_iam_role.verifier.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid      = "VerifierDlqSend"
        Effect   = "Allow"
        Action   = ["sqs:SendMessage"]
        Resource = aws_sqs_queue.verifier_dlq[0].arn
      },
    ]
  })
}

# ---------------------------------------------------------------------------------------------
# Lambda functions. Accepted, required artifacts (control_plane_package, verifier_package) — the
# repo ships binaries behind a feature flag, not packages; Terraform must never build Rust.
# ---------------------------------------------------------------------------------------------

resource "aws_lambda_function" "control_plane" {
  function_name = local.control_plane_function_name
  role          = aws_iam_role.control_plane.arn

  filename         = var.control_plane_package
  source_code_hash = filebase64sha256(var.control_plane_package)

  # `provided.al2023` is the custom-runtime contract lambda_http/lambda_runtime implement
  # directly (docs/DEPLOYMENT.md "Building the deployment artifacts"); `handler` is a required
  # field for this runtime family but its value is not otherwise interpreted.
  handler       = "bootstrap"
  runtime       = "provided.al2023"
  architectures = [var.architecture]

  memory_size = var.control_plane_memory_mb
  timeout     = var.control_plane_timeout_s

  ephemeral_storage {
    size = var.control_plane_ephemeral_storage_mb
  }

  environment {
    variables = local.control_plane_env
  }

  tags = var.tags

  depends_on = [
    aws_cloudwatch_log_group.control_plane,
    aws_iam_role_policy.control_plane,
  ]
}

resource "aws_lambda_function" "verifier" {
  function_name = local.verifier_function_name
  role          = aws_iam_role.verifier.arn

  filename         = var.verifier_package
  source_code_hash = filebase64sha256(var.verifier_package)

  handler       = "bootstrap"
  runtime       = "provided.al2023"
  architectures = [var.architecture]

  memory_size = var.verifier_memory_mb
  timeout     = var.verifier_timeout_s

  ephemeral_storage {
    size = var.verifier_ephemeral_storage_mb
  }

  environment {
    variables = local.verifier_env
  }

  dynamic "dead_letter_config" {
    for_each = var.create_verifier_dlq ? [1] : []

    content {
      target_arn = aws_sqs_queue.verifier_dlq[0].arn
    }
  }

  tags = var.tags

  depends_on = [
    aws_cloudwatch_log_group.verifier,
    aws_iam_role_policy.verifier,
  ]
}

# ---------------------------------------------------------------------------------------------
# API Gateway — HTTP API (v2). Created, no BYO, no REST option (§3.1, §7). A single catch-all
# route is sufficient (docs/DEPLOYMENT.md: entrypoint::handle does its own method/path dispatch
# and 404s on anything it does not recognize).
#
# Every resource below is `count`-gated on `var.create_api`, an internal test-harness variable
# (variables.tf) that exists solely because LocalStack community does not implement API Gateway
# v2 (design memo §8 risk 1) — Layer 2's LocalStack CI needs the bucket/DynamoDB/Lambda/event
# wiring without the gateway. The variable's own validation block makes `create_api = false`
# reachable only when `dev_endpoint_url` is also set, so this is structurally unreachable
# against real AWS.
# ---------------------------------------------------------------------------------------------

resource "aws_apigatewayv2_api" "this" {
  count = var.create_api ? 1 : 0

  name          = "${var.name_prefix}-api"
  protocol_type = "HTTP"
  tags          = var.tags
}

resource "aws_apigatewayv2_integration" "control_plane" {
  count = var.create_api ? 1 : 0

  api_id                 = aws_apigatewayv2_api.this[0].id
  integration_type       = "AWS_PROXY"
  integration_uri        = aws_lambda_function.control_plane.invoke_arn
  integration_method     = "POST"
  payload_format_version = "2.0"
}

resource "aws_apigatewayv2_route" "catch_all" {
  count = var.create_api ? 1 : 0

  api_id    = aws_apigatewayv2_api.this[0].id
  route_key = "ANY /{proxy+}"
  target    = "integrations/${aws_apigatewayv2_integration.control_plane[0].id}"
}

# Deployed to the literal `$default` stage with auto_deploy, and no variable admits any other
# value. This is not a stylistic choice: an HTTP API deployed to a *named* stage prepends
# `/{stage}` to every request path before routing, but entrypoint::handle matches absolute paths
# (`/v1/...`) — so a named stage would 404 every single request. A variable admitting only
# "$default" would be noise, and one admitting named stages ships a footgun that breaks
# everything. Pinned by C9.
resource "aws_apigatewayv2_stage" "default" {
  count = var.create_api ? 1 : 0

  api_id      = aws_apigatewayv2_api.this[0].id
  name        = "$default"
  auto_deploy = true

  default_route_settings {
    throttling_rate_limit  = var.throttling_rate_limit
    throttling_burst_limit = var.throttling_burst_limit
  }

  tags = var.tags
}

resource "aws_lambda_permission" "apigw_invoke" {
  count = var.create_api ? 1 : 0

  statement_id  = "AllowAPIGatewayInvoke"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.control_plane.function_name
  principal     = "apigateway.amazonaws.com"
  source_arn    = "${aws_apigatewayv2_api.this[0].execution_arn}/*/*"
}
