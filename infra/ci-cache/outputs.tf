output "bucket" {
  description = "The cache bucket: the SCCACHE_BUCKET repository secret."
  value       = aws_s3_bucket.cache.bucket
}

output "role_arn" {
  description = "The role CI assumes: the SCCACHE_ROLE_ARN repository secret."
  value       = aws_iam_role.sccache.arn
}
