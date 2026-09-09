resource "aws_s3_bucket" "packages" { bucket_prefix = "${var.name}-packages-" }
resource "aws_s3_bucket" "sessions" { bucket_prefix = "${var.name}-sessions-" }

resource "aws_s3_bucket_public_access_block" "blue" {
  for_each                = { packages = aws_s3_bucket.packages.id, sessions = aws_s3_bucket.sessions.id }
  bucket                  = each.value
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_server_side_encryption_configuration" "blue" {
  for_each = { packages = aws_s3_bucket.packages.id, sessions = aws_s3_bucket.sessions.id }
  bucket   = each.value
  rule {
    apply_server_side_encryption_by_default {
      kms_master_key_id = aws_kms_key.blue.arn
      sse_algorithm     = "aws:kms"
    }
    bucket_key_enabled = true
  }
}

resource "aws_s3_bucket_versioning" "packages" {
  bucket = aws_s3_bucket.packages.id
  versioning_configuration { status = "Enabled" }
}
resource "aws_s3_bucket_versioning" "sessions" {
  bucket = aws_s3_bucket.sessions.id
  versioning_configuration { status = "Enabled" }
}

resource "aws_s3_bucket_lifecycle_configuration" "sessions" {
  bucket     = aws_s3_bucket.sessions.id
  depends_on = [aws_s3_bucket_versioning.sessions]
  rule {
    id     = "expire-sessions"
    status = "Enabled"
    filter {}
    expiration { days = var.session_retention_days }
    noncurrent_version_expiration { noncurrent_days = var.session_retention_days }
    abort_incomplete_multipart_upload { days_after_initiation = 7 }
  }
  rule {
    id     = "remove-expired-delete-markers"
    status = "Enabled"
    filter {}
    expiration { expired_object_delete_marker = true }
  }
}
resource "aws_s3_bucket_lifecycle_configuration" "packages" {
  bucket     = aws_s3_bucket.packages.id
  depends_on = [aws_s3_bucket_versioning.packages]
  rule {
    id     = "package-housekeeping"
    status = "Enabled"
    filter {}
    noncurrent_version_expiration { noncurrent_days = var.package_noncurrent_retention_days }
    abort_incomplete_multipart_upload { days_after_initiation = 7 }
  }
}
