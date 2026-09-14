# Optional EKS cluster, count-guarded on local.create_cluster: leaving
# var.eks_cluster_name empty creates "<name>-<environment>", setting it attaches
# to a cluster someone else built. Downstream references use
# local.{oidc_issuer,oidc_provider_arn}, so the choice is invisible to the IRSA
# role below.
#
# Compute is EKS Auto Mode: AWS runs the node pools, the EBS CSI driver, and the
# load balancer controller, so nothing here declares a node group or an addon.
# Auto Mode is on only when compute, block storage, and elastic load balancing
# are all enabled together — a cluster with a subset of those is a standard
# cluster with no nodes.

data "aws_iam_policy_document" "eks_cluster_assume" {
  statement {
    # Auto Mode's control plane calls sts:TagSession when it assumes this role.
    actions = ["sts:AssumeRole", "sts:TagSession"]
    principals {
      type        = "Service"
      identifiers = ["eks.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "eks_cluster" {
  count              = local.create_cluster ? 1 : 0
  name_prefix        = "${local.iam_name}-cluster-"
  assume_role_policy = data.aws_iam_policy_document.eks_cluster_assume.json
}

resource "aws_iam_role_policy_attachment" "eks_cluster" {
  for_each = local.create_cluster ? toset([
    "arn:aws:iam::aws:policy/AmazonEKSClusterPolicy",
    "arn:aws:iam::aws:policy/AmazonEKSComputePolicy",
    "arn:aws:iam::aws:policy/AmazonEKSBlockStoragePolicy",
    "arn:aws:iam::aws:policy/AmazonEKSLoadBalancingPolicy",
    "arn:aws:iam::aws:policy/AmazonEKSNetworkingPolicy",
  ]) : []
  role       = aws_iam_role.eks_cluster[0].name
  policy_arn = each.value
}

# Envelope encryption of Kubernetes secrets needs the cluster role to hold
# grants on the key. No AWS managed policy carries this, and the key's own
# policy is the account default, so it is granted inline.
data "aws_iam_policy_document" "eks_cluster_kms" {
  statement {
    actions   = ["kms:DescribeKey", "kms:CreateGrant"]
    resources = [aws_kms_key.blue.arn]
  }
}

resource "aws_iam_role_policy" "eks_cluster_kms" {
  count       = local.create_cluster ? 1 : 0
  name_prefix = "secrets-encryption-"
  role        = aws_iam_role.eks_cluster[0].id
  policy      = data.aws_iam_policy_document.eks_cluster_kms.json
}

data "aws_iam_policy_document" "eks_node_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ec2.amazonaws.com"]
    }
  }
}

# Auto Mode nodes carry the "minimal" worker policy: the CNI and registry
# permissions the managed components need, and nothing else. Workload
# permissions belong on the IRSA role in main.tf, not here.
resource "aws_iam_role" "eks_node" {
  count              = local.create_cluster ? 1 : 0
  name_prefix        = "${local.iam_name}-node-"
  assume_role_policy = data.aws_iam_policy_document.eks_node_assume.json
}

resource "aws_iam_role_policy_attachment" "eks_node" {
  for_each = local.create_cluster ? toset([
    "arn:aws:iam::aws:policy/AmazonEKSWorkerNodeMinimalPolicy",
    "arn:aws:iam::aws:policy/AmazonEC2ContainerRegistryPullOnly",
  ]) : []
  role       = aws_iam_role.eks_node[0].name
  policy_arn = each.value
}

resource "aws_eks_cluster" "blue" {
  count    = local.create_cluster ? 1 : 0
  name     = local.name_prefix
  role_arn = aws_iam_role.eks_cluster[0].arn
  version  = var.kubernetes_version

  # Auto Mode supplies its own CNI, kube-proxy, and CoreDNS.
  bootstrap_self_managed_addons = false

  access_config {
    authentication_mode                         = "API"
    bootstrap_cluster_creator_admin_permissions = true
  }

  compute_config {
    enabled       = true
    node_pools    = var.cluster_node_pools
    node_role_arn = aws_iam_role.eks_node[0].arn
  }
  kubernetes_network_config {
    elastic_load_balancing { enabled = true }
  }
  storage_config {
    block_storage { enabled = true }
  }

  vpc_config {
    # Control plane ENIs land in the private subnets; Auto Mode places nodes in
    # the same set. Public subnets exist for ingress load balancers only.
    subnet_ids              = local.private_subnet_ids
    endpoint_private_access = true
    endpoint_public_access  = var.cluster_endpoint_public_access
    public_access_cidrs     = var.cluster_endpoint_public_access_cidrs
  }

  encryption_config {
    provider { key_arn = aws_kms_key.blue.arn }
    resources = ["secrets"]
  }

  lifecycle {
    precondition {
      condition     = length(local.private_subnet_ids) >= 2
      error_message = "EKS needs private subnets in at least two availability zones. Reusing a VPC (vpc_id set) means private_subnet_ids must list them."
    }
  }

  depends_on = [
    aws_iam_role_policy_attachment.eks_cluster,
    aws_iam_role_policy.eks_cluster_kms,
  ]
}

# IRSA needs an IAM OIDC provider for the cluster's issuer. An attached cluster
# is expected to have one already (the module reads it in main.tf); a created
# one does not, so it is created here. thumbprint_list is omitted deliberately:
# IAM derives it for issuers under a root CA it already trusts, which covers
# every EKS endpoint.
resource "aws_iam_openid_connect_provider" "eks" {
  count          = local.create_cluster ? 1 : 0
  url            = aws_eks_cluster.blue[0].identity[0].oidc[0].issuer
  client_id_list = ["sts.amazonaws.com"]
}
