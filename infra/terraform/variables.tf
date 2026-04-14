variable "aws_region" {
  description = "AWS region for the stack. This should stay us-east-1 because the CloudFront ACM certificate must live there."
  type        = string
  default     = "us-east-1"

  validation {
    condition     = var.aws_region == "us-east-1"
    error_message = "aws_region must be us-east-1 for the CloudFront certificate."
  }
}

variable "apex_domain" {
  description = "Canonical site domain."
  type        = string
  default     = "trueflow.dev"
}

variable "www_domain" {
  description = "WWW host that redirects to the apex domain."
  type        = string
  default     = "www.trueflow.dev"
}

variable "site_bucket_name" {
  description = "Bucket name for static site and download artifacts."
  type        = string
  default     = "trueflow.dev"
}

variable "tags" {
  description = "Optional AWS tags to apply to managed resources."
  type        = map(string)
  default     = {}
}
