# sccache's S3 cache for CI (.github/workflows/ci.yml): a private bucket whose objects expire,
# and a role that GitHub Actions runs of one repository assume through OIDC to read and write it.
# See README.md for the setup and teardown.

data "aws_caller_identity" "current" {}

# The account's GitHub OIDC provider already exists (other workflows sign in through it), and
# an account holds one provider per URL, so this root reads it rather than owning it: destroying
# this root must not sign the other workflows out.
data "aws_iam_openid_connect_provider" "github" {
  url = "https://token.actions.githubusercontent.com"
}

resource "aws_s3_bucket" "cache" {
  bucket = "forklift-sccache-${data.aws_caller_identity.current.account_id}"
}

resource "aws_s3_bucket_public_access_block" "cache" {
  bucket                  = aws_s3_bucket.cache.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_ownership_controls" "cache" {
  bucket = aws_s3_bucket.cache.id
  rule {
    object_ownership = "BucketOwnerEnforced"
  }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "cache" {
  bucket = aws_s3_bucket.cache.id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

resource "aws_s3_bucket_lifecycle_configuration" "cache" {
  bucket = aws_s3_bucket.cache.id
  rule {
    id     = "expire-cache-entries"
    status = "Enabled"
    filter {}
    expiration {
      days = var.expire_after_days
    }
    abort_incomplete_multipart_upload {
      days_after_initiation = 1
    }
  }
}

data "aws_iam_policy_document" "tls_only" {
  statement {
    sid     = "DenyInsecureTransport"
    effect  = "Deny"
    actions = ["s3:*"]
    resources = [
      aws_s3_bucket.cache.arn,
      "${aws_s3_bucket.cache.arn}/*",
    ]
    principals {
      type        = "*"
      identifiers = ["*"]
    }
    condition {
      test     = "Bool"
      variable = "aws:SecureTransport"
      values   = ["false"]
    }
  }
}

resource "aws_s3_bucket_policy" "cache" {
  bucket = aws_s3_bucket.cache.id
  policy = data.aws_iam_policy_document.tls_only.json
  # One after the other: concurrent configuration writes to one bucket can fail with a conflict.
  depends_on = [aws_s3_bucket_public_access_block.cache]
}

# Who may assume the role: runs of the repository's pull requests and of pushes to main, and
# nothing else of it (other branches, tags, environments). CI asks for the role only where the
# SCCACHE_ROLE_ARN secret is set, which GitHub does not pass to a pull request from a fork.
data "aws_iam_policy_document" "trust" {
  statement {
    effect  = "Allow"
    actions = ["sts:AssumeRoleWithWebIdentity"]
    principals {
      type        = "Federated"
      identifiers = [data.aws_iam_openid_connect_provider.github.arn]
    }
    condition {
      test     = "StringEquals"
      variable = "token.actions.githubusercontent.com:aud"
      values   = ["sts.amazonaws.com"]
    }
    condition {
      test     = "StringEquals"
      variable = "token.actions.githubusercontent.com:sub"
      values = [
        "repo:${var.github_repository}:pull_request",
        "repo:${var.github_repository}:ref:refs/heads/main",
      ]
    }
  }
}

resource "aws_iam_role" "sccache" {
  name                 = "forklift-ci-sccache"
  description          = "CI compile cache for ${var.github_repository}: read and write ${aws_s3_bucket.cache.bucket} only."
  assume_role_policy   = data.aws_iam_policy_document.trust.json
  max_session_duration = 3600
}

# Without ListBucket, S3 answers a read of a missing key with access-denied rather than
# not-found, so a cache miss would read as an error.
data "aws_iam_policy_document" "cache_access" {
  statement {
    actions   = ["s3:ListBucket"]
    resources = [aws_s3_bucket.cache.arn]
  }
  statement {
    actions   = ["s3:GetObject", "s3:PutObject"]
    resources = ["${aws_s3_bucket.cache.arn}/*"]
  }
}

resource "aws_iam_role_policy" "cache_access" {
  name   = "sccache-bucket"
  role   = aws_iam_role.sccache.id
  policy = data.aws_iam_policy_document.cache_access.json
}
