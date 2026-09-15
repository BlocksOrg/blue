# Dedicated native E2E infrastructure

This module is isolated from product deployments. Apply it in a dedicated test
account with an existing VPC, private subnet with outbound connectivity, and
GitHub OIDC provider. Supply `bucket_name`, `vpc_id`, `subnet_id`, `ami_id`, and
`github_oidc_provider_arn`. No resources are provisioned by checking in this module.

The amd64 AMI must include Docker, Compose >=2.24.4, AWS CLI, Python 3, a running SSM agent,
and at least 40 GiB root storage (delete on termination). Its instance role only
accesses the test bucket and SSM. It needs outbound access to container registries,
SSM, S3, and (gateway only) OpenRouter. The security group has no ingress rules.

Run `tofu init`, `tofu fmt -check`, `tofu validate`, then review `tofu plan` before
applying. Configure the GitHub `blue-e2e-native` environment with the output values
using the variable names in ../README.md. Restrict that environment to trusted
same-repository branches and reviewers: its OIDC subject authorizes remote code
on these disposable test instances. Never reuse a product deployment role.

Each instance has a three-hour expiry. A scheduled Lambda independently terminates
expired instances every 15 minutes; test objects expire after four hours in the
reaper and after one day by S3 lifecycle. Normal job cleanup removes both
immediately. Monitor Lambda failures. A stopped instance is still reaped.
