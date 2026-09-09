# Optional VPC. Everything here is count-guarded on local.create_vpc so that
# reusing an existing VPC (var.vpc_id set) provisions none of it. Downstream
# references use local.{vpc_id,public_subnet_ids,private_subnet_ids}.

resource "aws_vpc" "this" {
  count                = local.create_vpc ? 1 : 0
  cidr_block           = var.vpc_cidr
  enable_dns_hostnames = true
  enable_dns_support   = true
  tags                 = { Name = var.name }
}

resource "aws_subnet" "public" {
  count                   = local.create_vpc ? var.az_count : 0
  vpc_id                  = aws_vpc.this[0].id
  availability_zone       = local.az_names[count.index]
  cidr_block              = cidrsubnet(var.vpc_cidr, 4, count.index)
  map_public_ip_on_launch = true
  tags                    = { Name = "${var.name}-public-${local.az_names[count.index]}" }
}

resource "aws_subnet" "private" {
  count             = local.create_vpc ? var.az_count : 0
  vpc_id            = aws_vpc.this[0].id
  availability_zone = local.az_names[count.index]
  cidr_block        = cidrsubnet(var.vpc_cidr, 4, count.index + var.az_count)
  tags              = { Name = "${var.name}-private-${local.az_names[count.index]}" }
}

resource "aws_internet_gateway" "this" {
  count  = local.create_vpc ? 1 : 0
  vpc_id = aws_vpc.this[0].id
  tags   = { Name = var.name }
}

resource "aws_route_table" "public" {
  count  = local.create_vpc ? 1 : 0
  vpc_id = aws_vpc.this[0].id
  tags   = { Name = "${var.name}-public" }
}

resource "aws_route" "public_internet" {
  count                  = local.create_vpc ? 1 : 0
  route_table_id         = aws_route_table.public[0].id
  destination_cidr_block = "0.0.0.0/0"
  gateway_id             = aws_internet_gateway.this[0].id
}

resource "aws_route_table_association" "public" {
  count          = local.create_vpc ? var.az_count : 0
  subnet_id      = aws_subnet.public[count.index].id
  route_table_id = aws_route_table.public[0].id
}

# Single NAT gateway by default; one per AZ when single_nat_gateway = false.
locals {
  nat_count = local.create_vpc ? (var.single_nat_gateway ? 1 : var.az_count) : 0
}

resource "aws_eip" "nat" {
  count      = local.nat_count
  domain     = "vpc"
  tags       = { Name = "${var.name}-nat-${count.index}" }
  depends_on = [aws_internet_gateway.this]
}

resource "aws_nat_gateway" "this" {
  count         = local.nat_count
  allocation_id = aws_eip.nat[count.index].id
  subnet_id     = aws_subnet.public[count.index].id
  tags          = { Name = "${var.name}-nat-${count.index}" }
  depends_on    = [aws_internet_gateway.this]
}

resource "aws_route_table" "private" {
  count  = local.create_vpc ? var.az_count : 0
  vpc_id = aws_vpc.this[0].id
  tags   = { Name = "${var.name}-private-${count.index}" }
}

resource "aws_route" "private_nat" {
  count                  = local.create_vpc ? var.az_count : 0
  route_table_id         = aws_route_table.private[count.index].id
  destination_cidr_block = "0.0.0.0/0"
  nat_gateway_id         = var.single_nat_gateway ? aws_nat_gateway.this[0].id : aws_nat_gateway.this[count.index].id
}

resource "aws_route_table_association" "private" {
  count          = local.create_vpc ? var.az_count : 0
  subnet_id      = aws_subnet.private[count.index].id
  route_table_id = aws_route_table.private[count.index].id
}
