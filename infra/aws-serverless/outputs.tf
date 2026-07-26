# §3.3 of the design memo. Semver-governed alongside variables.tf — renames and removals are
# breaking changes.

output "api_endpoint" {
  description = "The HTTP API's invoke URL (the $default stage's own endpoint — see main.tf on why the stage cannot be named)."
  value       = aws_apigatewayv2_api.this.api_endpoint
}

output "api_id" {
  description = "The HTTP API's id — the sanctioned extension seam for a custom-domain API mapping (§7: attach aws_apigatewayv2_domain_name + an API mapping to this id from outside the module, with an empty mapping key)."
  value       = aws_apigatewayv2_api.this.id
}

output "bucket_name" {
  description = "The S3 bucket name."
  value       = aws_s3_bucket.this.bucket
}

output "bucket_arn" {
  description = "The S3 bucket ARN."
  value       = aws_s3_bucket.this.arn
}

output "table_name" {
  description = "The DynamoDB table name."
  value       = aws_dynamodb_table.this.name
}

output "table_arn" {
  description = "The DynamoDB table ARN."
  value       = aws_dynamodb_table.this.arn
}

output "control_plane_function_name" {
  description = "The control-plane Lambda function name."
  value       = aws_lambda_function.control_plane.function_name
}

output "control_plane_function_arn" {
  description = "The control-plane Lambda function ARN."
  value       = aws_lambda_function.control_plane.arn
}

output "verifier_function_name" {
  description = "The verifier Lambda function name."
  value       = aws_lambda_function.verifier.function_name
}

output "verifier_function_arn" {
  description = "The verifier Lambda function ARN."
  value       = aws_lambda_function.verifier.arn
}

output "control_plane_role_arn" {
  description = "The control-plane Lambda execution role ARN — the sanctioned seam for attaching extra policies externally (§3.1: BYO roles risk deploying without the derived policy, so this output plus permissions_boundary_arn is the escape hatch instead)."
  value       = aws_iam_role.control_plane.arn
}

output "verifier_role_arn" {
  description = "The verifier Lambda execution role ARN — same rationale as control_plane_role_arn."
  value       = aws_iam_role.verifier.arn
}

output "verifier_dlq_url" {
  description = "The verifier dead-letter queue's URL, or null when create_verifier_dlq is false."
  value       = var.create_verifier_dlq ? aws_sqs_queue.verifier_dlq[0].url : null
}

output "verifier_dlq_arn" {
  description = "The verifier dead-letter queue's ARN, or null when create_verifier_dlq is false."
  value       = var.create_verifier_dlq ? aws_sqs_queue.verifier_dlq[0].arn : null
}

output "control_plane_log_group_name" {
  description = "The control-plane Lambda's CloudWatch log group name."
  value       = aws_cloudwatch_log_group.control_plane.name
}

output "verifier_log_group_name" {
  description = "The verifier Lambda's CloudWatch log group name."
  value       = aws_cloudwatch_log_group.verifier.name
}
