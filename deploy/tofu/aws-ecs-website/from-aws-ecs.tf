# One-time upgrade from the full aws-ecs stack. Safe to delete once applied.

# The website resources were optional (count) in aws-ecs; here they always exist.
moved {
  from = aws_cloudwatch_log_group.website[0]
  to   = aws_cloudwatch_log_group.website
}
moved {
  from = aws_ecs_task_definition.website[0]
  to   = aws_ecs_task_definition.website
}
moved {
  from = aws_ecs_service.website[0]
  to   = aws_ecs_service.website
}
moved {
  from = aws_lb_target_group.website[0]
  to   = aws_lb_target_group.website
}

# The package and session buckets hold data, so Tofu stops tracking them
# instead of deleting them. Empty and delete them by hand once nothing needs it.
# They stay encrypted with aws_kms_key.blue, which is why that key is kept.
removed {
  from = aws_s3_bucket.packages
  lifecycle { destroy = false }
}
removed {
  from = aws_s3_bucket.sessions
  lifecycle { destroy = false }
}
removed {
  from = aws_s3_bucket_public_access_block.blue
  lifecycle { destroy = false }
}
removed {
  from = aws_s3_bucket_server_side_encryption_configuration.blue
  lifecycle { destroy = false }
}
removed {
  from = aws_s3_bucket_versioning.packages
  lifecycle { destroy = false }
}
removed {
  from = aws_s3_bucket_versioning.sessions
  lifecycle { destroy = false }
}
removed {
  from = aws_s3_bucket_lifecycle_configuration.packages
  lifecycle { destroy = false }
}
removed {
  from = aws_s3_bucket_lifecycle_configuration.sessions
  lifecycle { destroy = false }
}
