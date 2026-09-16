terraform {
  required_version = ">= 1.8"
  required_providers {
    aws     = { source = "hashicorp/aws", version = "~> 5.0" }
    archive = { source = "hashicorp/archive", version = "~> 2.7" }
  }
}
data "aws_caller_identity" "current" {}
data "aws_region" "current" {}
data "aws_partition" "current" {}
locals {
  arn = "arn:${data.aws_partition.current.partition}"
  ec2 = "${local.arn}:ec2:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}"
  ssm = "${local.arn}:ssm:${data.aws_region.current.name}"
}
resource "aws_s3_bucket" "test" {
  bucket = var.bucket_name
  tags   = { BlueE2E = "native" }
}
resource "aws_s3_bucket_public_access_block" "test" {
  bucket                  = aws_s3_bucket.test.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}
resource "aws_s3_bucket_server_side_encryption_configuration" "test" {
  bucket = aws_s3_bucket.test.id
  rule {
    apply_server_side_encryption_by_default { sse_algorithm = "AES256" }
  }
}
resource "aws_s3_bucket_lifecycle_configuration" "test" {
  bucket = aws_s3_bucket.test.id
  rule {
    id     = "expire-abandoned-runs"
    status = "Enabled"
    filter { prefix = "runs/" }
    expiration { days = 1 }
    abort_incomplete_multipart_upload { days_after_initiation = 1 }
  }
}
resource "aws_s3_bucket_policy" "tls" {
  bucket = aws_s3_bucket.test.id
  policy = jsonencode({ Version = "2012-10-17", Statement = [{ Effect = "Deny", Principal = "*", Action = "s3:*", Resource = [aws_s3_bucket.test.arn, "${aws_s3_bucket.test.arn}/*"], Condition = { Bool = { "aws:SecureTransport" = "false" } } }] })
}
resource "aws_security_group" "backend" {
  name_prefix = "${var.name}-"
  description = "Disposable E2E backend: no inbound access; SSM tunnels only"
  vpc_id      = var.vpc_id
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
  tags = { BlueE2E = "native" }
}
resource "aws_iam_role" "instance" {
  name               = "${var.name}-instance"
  assume_role_policy = jsonencode({ Version = "2012-10-17", Statement = [{ Effect = "Allow", Principal = { Service = "ec2.amazonaws.com" }, Action = "sts:AssumeRole" }] })
}
resource "aws_iam_role_policy_attachment" "ssm" {
  role       = aws_iam_role.instance.name
  policy_arn = "${local.arn}:iam::aws:policy/AmazonSSMManagedInstanceCore"
}
resource "aws_iam_instance_profile" "backend" {
  name = "${var.name}-instance"
  role = aws_iam_role.instance.name
}
resource "aws_iam_role_policy" "instance_storage" {
  role = aws_iam_role.instance.id
  policy = jsonencode({ Version = "2012-10-17", Statement = [
    { Effect = "Allow", Action = ["s3:ListBucket"], Resource = aws_s3_bucket.test.arn, Condition = { StringLike = { "s3:prefix" = "runs/*" } } },
    { Effect = "Allow", Action = ["s3:GetObject", "s3:PutObject"], Resource = "${aws_s3_bucket.test.arn}/runs/*" }
  ] })
}
resource "aws_iam_role" "github" {
  name                 = "${var.name}-github"
  max_session_duration = 7200
  assume_role_policy = jsonencode({ Version = "2012-10-17", Statement = [{ Effect = "Allow", Principal = { Federated = var.github_oidc_provider_arn }, Action = "sts:AssumeRoleWithWebIdentity", Condition = { StringEquals = {
    "token.actions.githubusercontent.com:aud" = "sts.amazonaws.com",
    "token.actions.githubusercontent.com:sub" = "repo:${var.github_repository}:environment:${var.github_environment}"
  } } }] })
}
resource "aws_iam_role_policy" "github" {
  role = aws_iam_role.github.id
  policy = jsonencode({ Version = "2012-10-17", Statement = [
    { Effect = "Allow", Action = ["s3:ListBucket"], Resource = aws_s3_bucket.test.arn, Condition = { StringLike = { "s3:prefix" = "runs/*" } } },
    { Effect = "Allow", Action = ["s3:GetObject", "s3:PutObject", "s3:DeleteObject", "s3:AbortMultipartUpload"], Resource = "${aws_s3_bucket.test.arn}/runs/*" },
    { Effect = "Allow", Action = ["ec2:RunInstances"], Resource = ["${local.ec2}:instance/*", "${local.ec2}:volume/*"], Condition = { StringEquals = { "aws:RequestTag/BlueE2E" = "native" } } },
    { Effect = "Allow", Action = ["ec2:RunInstances"], Resource = ["${local.arn}:ec2:${data.aws_region.current.name}::image/${var.ami_id}", "${local.ec2}:subnet/${var.subnet_id}", aws_security_group.backend.arn, "${local.ec2}:network-interface/*"] },
    { Effect = "Allow", Action = ["ec2:CreateTags"], Resource = ["${local.ec2}:instance/*", "${local.ec2}:volume/*"], Condition = { StringEquals = { "ec2:CreateAction" = "RunInstances" } } },
    { Effect = "Allow", Action = ["ec2:TerminateInstances"], Resource = "${local.ec2}:instance/*", Condition = { StringEquals = { "ec2:ResourceTag/BlueE2E" = "native" } } },
    { Effect = "Allow", Action = ["ec2:DescribeInstances", "ssm:DescribeInstanceInformation", "ssm:GetCommandInvocation"], Resource = "*" },
    { Effect = "Allow", Action = ["iam:PassRole"], Resource = aws_iam_role.instance.arn, Condition = { StringEquals = { "iam:PassedToService" = "ec2.amazonaws.com" } } },
    { Effect = "Allow", Action = ["ssm:SendCommand", "ssm:StartSession"], Resource = "${local.ec2}:instance/*", Condition = { StringEquals = { "ssm:resourceTag/BlueE2E" = "native" } } },
    { Effect = "Allow", Action = ["ssm:SendCommand"], Resource = "${local.ssm}::document/AWS-RunShellScript" },
    { Effect = "Allow", Action = ["ssm:StartSession"], Resource = "${local.ssm}::document/AWS-StartPortForwardingSession" },
    { Effect = "Allow", Action = ["ssm:TerminateSession"], Resource = "${local.ssm}:${data.aws_caller_identity.current.account_id}:session/*" }
  ] })
}
# Independent of the GitHub job, including cancelled jobs and hard runner loss.
data "archive_file" "reaper" {
  type        = "zip"
  source_file = "${path.module}/reaper.py"
  output_path = "${path.module}/.terraform/reaper.zip"
}
resource "aws_iam_role" "reaper" {
  name               = "${var.name}-reaper"
  assume_role_policy = jsonencode({ Version = "2012-10-17", Statement = [{ Effect = "Allow", Principal = { Service = "lambda.amazonaws.com" }, Action = "sts:AssumeRole" }] })
}
resource "aws_iam_role_policy_attachment" "reaper_logs" {
  role       = aws_iam_role.reaper.name
  policy_arn = "${local.arn}:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}
resource "aws_iam_role_policy" "reaper" {
  role = aws_iam_role.reaper.id
  policy = jsonencode({ Version = "2012-10-17", Statement = [
    { Effect = "Allow", Action = ["ec2:DescribeInstances"], Resource = "*" },
    { Effect = "Allow", Action = ["ec2:TerminateInstances"], Resource = "${local.ec2}:instance/*", Condition = { StringEquals = { "ec2:ResourceTag/BlueE2E" = "native" } } },
    { Effect = "Allow", Action = ["s3:ListBucket"], Resource = aws_s3_bucket.test.arn, Condition = { StringLike = { "s3:prefix" = "runs/*" } } },
    { Effect = "Allow", Action = ["s3:DeleteObject"], Resource = "${aws_s3_bucket.test.arn}/runs/*" }
  ] })
}
resource "aws_lambda_function" "reaper" {
  function_name    = "${var.name}-reaper"
  role             = aws_iam_role.reaper.arn
  runtime          = "python3.12"
  handler          = "reaper.handler"
  timeout          = 120
  filename         = data.archive_file.reaper.output_path
  source_code_hash = data.archive_file.reaper.output_base64sha256
  environment { variables = { BUCKET = aws_s3_bucket.test.id } }
}
resource "aws_cloudwatch_event_rule" "reaper" {
  name                = "${var.name}-reaper"
  schedule_expression = "rate(15 minutes)"
}
resource "aws_cloudwatch_event_target" "reaper" {
  rule = aws_cloudwatch_event_rule.reaper.name
  arn  = aws_lambda_function.reaper.arn
}
resource "aws_lambda_permission" "reaper" {
  statement_id  = "ScheduledReaper"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.reaper.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.reaper.arn
}
