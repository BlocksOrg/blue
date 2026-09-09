resource "aws_kms_key" "blue" {
  description             = "Blue deployment data"
  deletion_window_in_days = 30
  enable_key_rotation     = true
}

resource "aws_kms_alias" "blue" {
  name          = "alias/${var.name}"
  target_key_id = aws_kms_key.blue.key_id
}
