# A complete, deployable root configuration — this file doubles as the README quickstart.
# Copy this directory, adjust the two package paths and the region, and `tofu apply`
# (or `terraform apply`; both work, see the module's README.md).

terraform {
  required_version = ">= 1.8.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
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
  description = "The bearer token the deployed head will require."
  type        = string
  sensitive   = true
}

module "forklift" {
  source = "../.."

  control_plane_package = var.control_plane_package
  verifier_package      = var.verifier_package
  auth_token            = var.auth_token
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
