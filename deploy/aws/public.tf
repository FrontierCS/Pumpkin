# The public page: the controller's runs as static files in a private bucket, which a CloudFront
# distribution alone reads and serves at its own address, and which the controller's role writes.
# README.md says what the person does with it.

resource "aws_s3_bucket" "public" {
  count = var.public_page ? 1 : 0

  bucket_prefix = "${local.prefix}-public-"
  # Its files are copies of the controller's records, so a destroy may empty it.
  force_destroy = true
}

# CloudFront alone reads the bucket, by the bucket policy below, so no one reaches it directly.
resource "aws_s3_bucket_public_access_block" "public" {
  count = var.public_page ? 1 : 0

  bucket                  = aws_s3_bucket.public[0].id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_cloudfront_origin_access_control" "public" {
  count = var.public_page ? 1 : 0

  name                              = "${local.prefix}-public"
  description                       = "Pumpkin's public page: CloudFront's reads of its bucket"
  origin_access_control_origin_type = "s3"
  signing_behavior                  = "always"
  signing_protocol                  = "sigv4"
}

# The page sets its own content security policy in a `<meta>`, so none is here.
resource "aws_cloudfront_response_headers_policy" "public" {
  count = var.public_page ? 1 : 0

  name    = "${local.prefix}-public"
  comment = "Pumpkin's public page: no sniffing, no framing, HTTPS, no indexing"

  security_headers_config {
    content_type_options {
      override = true
    }
    frame_options {
      frame_option = "DENY"
      override     = true
    }
    strict_transport_security {
      access_control_max_age_sec = 31536000
      override                   = true
    }
  }

  # Search engines are kept out: the address is shared within a team, never advertised.
  custom_headers_config {
    items {
      header   = "X-Robots-Tag"
      value    = "noindex"
      override = true
    }
  }
}

# Each file's lifetime is the `Cache-Control` its upload sets, which this policy honours.
data "aws_cloudfront_cache_policy" "optimized" {
  count = var.public_page ? 1 : 0

  name = "Managed-CachingOptimized"
}

resource "aws_cloudfront_distribution" "public" {
  count = var.public_page ? 1 : 0

  enabled             = true
  comment             = "Pumpkin's public page of ${local.prefix}"
  default_root_object = "index.html"
  # North America and Europe alone, the cheapest class: elsewhere is served from those.
  price_class = "PriceClass_100"

  origin {
    origin_id                = "bucket"
    domain_name              = aws_s3_bucket.public[0].bucket_regional_domain_name
    origin_access_control_id = aws_cloudfront_origin_access_control.public[0].id
  }

  default_cache_behavior {
    target_origin_id           = "bucket"
    viewer_protocol_policy     = "redirect-to-https"
    allowed_methods            = ["GET", "HEAD"]
    cached_methods             = ["GET", "HEAD"]
    cache_policy_id            = data.aws_cloudfront_cache_policy.optimized[0].id
    response_headers_policy_id = aws_cloudfront_response_headers_policy.public[0].id
    # The deflated files are stored with their `Content-Encoding`: CloudFront compresses none
    # itself.
    compress = false
  }

  restrictions {
    geo_restriction {
      restriction_type = "none"
    }
  }

  viewer_certificate {
    cloudfront_default_certificate = true
  }
}

resource "aws_s3_bucket_policy" "public" {
  count = var.public_page ? 1 : 0

  bucket = aws_s3_bucket.public[0].id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "cloudfront.amazonaws.com" }
      Action    = "s3:GetObject"
      Resource  = "${aws_s3_bucket.public[0].arn}/*"
      Condition = { StringEquals = { "AWS:SourceArn" = aws_cloudfront_distribution.public[0].arn } }
    }]
  })

  # S3 refuses a second change to one bucket's settings while another is in progress.
  depends_on = [aws_s3_bucket_public_access_block.public]
}

# A publish: `aws s3 sync`'s list of the bucket, and its uploads.
resource "aws_iam_role_policy" "controller_publish" {
  count = var.public_page ? 1 : 0

  name = "publish"
  role = aws_iam_role.controller.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      { Effect = "Allow", Action = "s3:ListBucket", Resource = aws_s3_bucket.public[0].arn },
      { Effect = "Allow", Action = "s3:PutObject", Resource = "${aws_s3_bucket.public[0].arn}/*" },
    ]
  })
}

# The settings' `[publish]` table, as `pumpkin help config` says.
locals {
  publish_settings = flatten([for distribution in aws_cloudfront_distribution.public : [
    "",
    "[publish]",
    "bucket = ${jsonencode(aws_s3_bucket.public[0].bucket)}",
    "distribution = ${jsonencode(distribution.id)}",
    "address = ${jsonencode("https://${distribution.domain_name}/")}",
    # An offset date-time, which TOML writes bare: quoted, it would be a string.
    var.publish_since == "" ? [] : ["since = ${var.publish_since}"],
  ] if var.publish_from_jobs])
}
