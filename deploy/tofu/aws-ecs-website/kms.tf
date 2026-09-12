resource "aws_kms_key" "blue" {
  description             = "Blue deployment data"
  deletion_window_in_days = 30
  enable_key_rotation     = true
}

resource "aws_kms_alias" "blue" {
  name          = "alias/${var.name}"
  target_key_id = aws_kms_key.blue.key_id
}

# CloudWatch Logs encrypts log groups itself, as the service principal, so the
# default key policy (account root only) is not enough: CreateLogGroup fails
# with "The specified KMS key does not exist or is not allowed to be used".
# The service grant is scoped by encryption context to this deployment's log
# groups. Every other consumer (RDS, ElastiCache, S3, Secrets Manager) calls
# KMS as an account IAM principal and stays covered by the root statement.
data "aws_iam_policy_document" "kms" {
  statement {
    sid       = "EnableIAMUserPermissions"
    actions   = ["kms:*"]
    resources = ["*"]
    principals {
      type        = "AWS"
      identifiers = ["arn:${data.aws_partition.current.partition}:iam::${data.aws_caller_identity.current.account_id}:root"]
    }
  }

  statement {
    sid = "AllowCloudWatchLogs"
    actions = [
      "kms:Encrypt*",
      "kms:Decrypt*",
      "kms:ReEncrypt*",
      "kms:GenerateDataKey*",
      "kms:Describe*",
    ]
    resources = ["*"]
    principals {
      type        = "Service"
      identifiers = ["logs.${var.aws_region}.amazonaws.com"]
    }
    condition {
      test     = "ArnLike"
      variable = "kms:EncryptionContext:aws:logs:arn"
      values   = ["arn:${data.aws_partition.current.partition}:logs:${var.aws_region}:${data.aws_caller_identity.current.account_id}:log-group:/ecs/${var.name}/*"]
    }
  }
}

resource "aws_kms_key_policy" "blue" {
  key_id = aws_kms_key.blue.id
  policy = data.aws_iam_policy_document.kms.json
}
