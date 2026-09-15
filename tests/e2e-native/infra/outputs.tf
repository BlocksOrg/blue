output "github_role_arn" { value = aws_iam_role.github.arn }
output "bucket" { value = aws_s3_bucket.test.id }
output "instance_profile" { value = aws_iam_instance_profile.backend.name }
output "security_group_id" { value = aws_security_group.backend.id }
output "subnet_id" { value = var.subnet_id }
output "ami_id" { value = var.ami_id }
