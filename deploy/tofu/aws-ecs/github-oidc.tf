# ---------------------------------------------------------------------------
# GitHub Actions OIDC — keyless deploy role for .github/workflows/website-deploy.yml.
#
# The workflow assumes this role with a short-lived GitHub OIDC token (no
# long-lived access keys), then registers a task-definition revision and updates
# the ECS service. Permissions come from AWS managed policies only.
#
# The token.actions.githubusercontent.com provider is account-global and shared
# by every stack in the account, so this module references it rather than owning
# it. Create it once per account if it is missing:
#
#   aws iam create-open-id-connect-provider \
#     --url https://token.actions.githubusercontent.com \
#     --client-id-list sts.amazonaws.com
# ---------------------------------------------------------------------------
locals {
  enable_github_oidc = var.github_oidc_repository != ""

  # Fixed provider URL ⇒ the ARN is derivable, so it needs no input.
  github_oidc_provider_arn = "arn:${data.aws_partition.current.partition}:iam::${data.aws_caller_identity.current.account_id}:oidc-provider/token.actions.githubusercontent.com"

  # Only this stack's environment — not every branch, tag, or pull request in
  # the repository. var.environment must equal the `environment:` key on the
  # workflow job, which is what GitHub puts in the token's sub claim.
  #
  # A repository created after 2026-07-15 (or an older one opted in) issues an
  # immutable subject claim, splicing the numeric owner and repository IDs into
  # the subject so a rename cannot transfer trust — see github_oidc_repository.
  # AWS accepts only the `sub` and `aud` claims in a trust policy, so the
  # subject is the sole lever; there is no conditioning on repository_id.
  github_oidc_sub = "repo:${var.github_oidc_repository}:environment:${var.environment}"

  # AmazonECS_FullAccess covers describe-services / describe-task-definition /
  # register-task-definition / update-service plus the iam:PassRole the new
  # revision needs for the task and execution roles (scoped by the managed
  # policy to ecs-tasks.amazonaws.com).
  github_oidc_policy_arn = "arn:${data.aws_partition.current.partition}:iam::aws:policy/AmazonECS_FullAccess"
}

data "aws_iam_policy_document" "github_actions_assume" {
  count = local.enable_github_oidc ? 1 : 0

  statement {
    actions = ["sts:AssumeRoleWithWebIdentity"]
    principals {
      type        = "Federated"
      identifiers = [local.github_oidc_provider_arn]
    }
    condition {
      test     = "StringEquals"
      variable = "token.actions.githubusercontent.com:aud"
      values   = ["sts.amazonaws.com"]
    }
    condition {
      test     = "StringEquals"
      variable = "token.actions.githubusercontent.com:sub"
      values   = [local.github_oidc_sub]
    }
  }
}

resource "aws_iam_role" "github_actions" {
  count              = local.enable_github_oidc ? 1 : 0
  name_prefix        = "${var.name}-gha-"
  description        = "GitHub Actions deploy role for ${var.github_oidc_repository}"
  assume_role_policy = data.aws_iam_policy_document.github_actions_assume[0].json
}

resource "aws_iam_role_policy_attachment" "github_actions" {
  count      = local.enable_github_oidc ? 1 : 0
  role       = aws_iam_role.github_actions[0].name
  policy_arn = local.github_oidc_policy_arn
}
