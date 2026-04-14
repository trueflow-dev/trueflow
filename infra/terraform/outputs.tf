output "site_bucket_name" {
  description = "Name of the private S3 bucket behind CloudFront."
  value       = aws_s3_bucket.site.bucket
}

output "site_distribution_id" {
  description = "CloudFront distribution id for cache invalidation and inspection."
  value       = aws_cloudfront_distribution.site.id
}

output "site_distribution_domain_name" {
  description = "CloudFront distribution hostname."
  value       = aws_cloudfront_distribution.site.domain_name
}

output "site_apex_url" {
  description = "Canonical website URL."
  value       = "https://${var.apex_domain}"
}
