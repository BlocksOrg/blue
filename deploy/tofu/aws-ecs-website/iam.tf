# ---------------------------------------------------------------------------
# Task execution role — pulls the image and writes logs. The website needs no
# task role: it calls no AWS APIs.
# ---------------------------------------------------------------------------
data "aws_iam_policy_document" "execution_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ecs-tasks.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "execution" {
  name_prefix        = "${var.name}-exec-"
  assume_role_policy = data.aws_iam_policy_document.execution_assume.json
}

resource "aws_iam_role_policy_attachment" "execution_managed" {
  role       = aws_iam_role.execution.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy"
}

# Registry credentials for a private image, when one is configured.
data "aws_iam_policy_document" "execution" {
  count = var.image_pull_secret_arn != "" ? 1 : 0
  statement {
    sid       = "ReadImagePullSecret"
    actions   = ["secretsmanager:GetSecretValue"]
    resources = [var.image_pull_secret_arn]
  }
}

resource "aws_iam_role_policy" "execution" {
  count  = var.image_pull_secret_arn != "" ? 1 : 0
  role   = aws_iam_role.execution.id
  policy = data.aws_iam_policy_document.execution[0].json
}
