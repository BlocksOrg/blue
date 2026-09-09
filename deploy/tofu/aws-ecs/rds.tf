resource "aws_db_subnet_group" "blue" {
  name       = var.name
  subnet_ids = local.private_subnet_ids
}

data "aws_iam_policy_document" "rds_monitoring_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["monitoring.rds.amazonaws.com"]
    }
  }
}
resource "aws_iam_role" "rds_monitoring" {
  name_prefix        = "${var.name}-rds-monitoring-"
  assume_role_policy = data.aws_iam_policy_document.rds_monitoring_assume.json
}
resource "aws_iam_role_policy_attachment" "rds_monitoring" {
  role       = aws_iam_role.rds_monitoring.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonRDSEnhancedMonitoringRole"
}

resource "aws_db_instance" "blue" {
  identifier                          = local.resource_name
  engine                              = "postgres"
  engine_version                      = "16"
  instance_class                      = var.database_instance_class
  allocated_storage                   = var.database_allocated_storage
  storage_encrypted                   = true
  kms_key_id                          = aws_kms_key.blue.arn
  db_name                             = var.database_name
  username                            = var.database_username
  password                            = random_password.database.result
  db_subnet_group_name                = aws_db_subnet_group.blue.name
  vpc_security_group_ids              = [aws_security_group.database.id]
  backup_retention_period             = var.database_backup_retention_days
  copy_tags_to_snapshot               = true
  deletion_protection                 = var.deletion_protection
  skip_final_snapshot                 = !var.deletion_protection
  final_snapshot_identifier           = var.deletion_protection ? "${local.resource_name}-final" : null
  auto_minor_version_upgrade          = true
  publicly_accessible                 = false
  multi_az                            = true
  iam_database_authentication_enabled = true
  enabled_cloudwatch_logs_exports     = ["postgresql", "upgrade"]
  monitoring_interval                 = 60
  monitoring_role_arn                 = aws_iam_role.rds_monitoring.arn
  performance_insights_enabled        = true
  performance_insights_kms_key_id     = aws_kms_key.blue.arn
}
