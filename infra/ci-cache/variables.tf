variable "region" {
  description = "Region of the cache bucket."
  type        = string
  default     = "eu-central-1"
}

variable "github_repository" {
  description = "The owner/name of the repository whose CI may use the cache."
  type        = string
  default     = "lonic-software/forklift"
}

variable "expire_after_days" {
  description = "Days after a cache object is written before it is deleted, in use or not. This bounds the bucket's size; an expired entry costs one miss, after which sccache writes it again."
  type        = number
  default     = 14
}
