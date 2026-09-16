variable "name" {
  type    = string
  default = "blue-e2e-native"
}
variable "bucket_name" { type = string }
variable "vpc_id" { type = string }
variable "subnet_id" { type = string }
variable "ami_id" {
  type        = string
  description = "Dedicated Linux amd64 AMI with Docker, Compose >=2.24.4, AWS CLI and running SSM agent; root volume >=40 GiB, outbound connectivity required."
}
variable "github_oidc_provider_arn" { type = string }
variable "github_repository" {
  type    = string
  default = "BlocksOrg/blue"
}
variable "github_environment" {
  type    = string
  default = "blue-e2e-native"
}
