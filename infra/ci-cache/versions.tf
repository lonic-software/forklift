# The same toolchain floor and provider address as infra/aws-serverless (see its versions.tf for
# why the provider source names registry.opentofu.org in full).
terraform {
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
