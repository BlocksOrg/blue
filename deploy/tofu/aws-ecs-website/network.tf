# Optional VPC with public subnets only. The website tasks get their own public
# IPs and reach the internet through the internet gateway, so there is no NAT
# gateway and no Elastic IP. Everything here is count-guarded on
# local.create_vpc so reusing an existing VPC (var.vpc_id set) creates none of it.

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
