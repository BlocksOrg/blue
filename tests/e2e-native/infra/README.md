# Dedicated native E2E infrastructure

This module is isolated from product deployments. Apply it in a dedicated test
account with an existing VPC, private subnet with outbound connectivity, and
GitHub OIDC provider. Supply `bucket_name`, `vpc_id`, `subnet_id`, `ami_id`, and
`github_oidc_provider_arn`. No resources are provisioned by checking in this module.

The amd64 AMI must include Docker, Compose >=2.24.4, AWS CLI, Python 3, a running SSM agent,
and at least 40 GiB root storage (delete on termination). Its instance role only
accesses the test bucket and SSM. It needs outbound access to container registries,
SSM, S3, and (gateway only) OpenRouter. The security group has no ingress rules.

## Manual runner configuration

Inspect existing test resources/state before provisioning. Run `tofu init`,
`tofu fmt -check`, `tofu validate`, then review `tofu plan` before applying.
Export the module outputs in the shell on the Windows test machine:

| Module output | Manual runner environment variable |
| --- | --- |
| `region` | `AWS_REGION` |
| `bucket` | `E2E_NATIVE_BUCKET` |
| `subnet_id` | `E2E_NATIVE_SUBNET_ID` |
| `security_group_id` | `E2E_NATIVE_SECURITY_GROUP_ID` |
| `instance_profile` | `E2E_NATIVE_INSTANCE_PROFILE` |
| `ami_id` | `E2E_NATIVE_AMI_ID` |

Run `node tests/e2e-native/preflight.mjs` from the repository root to check the
five backend settings. Authenticate separately through the normal AWS credential
chain with dedicated-test permissions to manage the leased backend. Never reuse
a product deployment role. Set `OPENROUTER_API_KEY` locally only for gateway runs;
without it gateway remains unverified.

The module retains its GitHub OIDC role and `github_role_arn` output, but no
native E2E workflow currently uses them. That role trusts only
`repo:BlocksOrg/blue:environment:blue-e2e-native` with audience
`sts.amazonaws.com` and maximum session duration 7200 seconds; it does not grant
manual users access. The retained `preflight.mjs --ci` mode checks the additional
`E2E_NATIVE_ROLE_ARN` and `E2E_NATIVE_REGION` inputs for potential future automation.
Neither a GitHub environment nor those CI-only variables is needed to run the
manual runner with separately authorized AWS credentials.

Each instance has a three-hour expiry. A scheduled Lambda independently terminates
expired instances every 15 minutes; test objects expire after four hours in the
reaper and after one day by S3 lifecycle. Normal job cleanup removes both
immediately. Monitor Lambda failures. A stopped instance is still reaped.
