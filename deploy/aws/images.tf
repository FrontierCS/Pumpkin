# The private repositories that runs push to and sandboxes pull from: the roles' images, and the
# images that each hold a static `sandboxd` and `ec2-boot`. Tags can't move, as the `aws` crate
# pushes each image tagged with its digest. A destroy deletes them with their images.
resource "aws_ecr_repository" "images" {
  for_each = toset(["roles", "sandboxd"])

  name                 = "${local.prefix}/${each.key}"
  image_tag_mutability = "IMMUTABLE"
  force_delete         = true
}

# Pumpkin pushes every image tagged, so this deletes only images orphaned outside it, 14 days on.
resource "aws_ecr_lifecycle_policy" "images" {
  for_each = aws_ecr_repository.images

  repository = each.value.name
  policy = jsonencode({
    rules = [{
      rulePriority = 1
      description  = "Expire untagged images 14 days after their push"
      selection = {
        tagStatus   = "untagged"
        countType   = "sinceImagePushed"
        countUnit   = "days"
        countNumber = 14
      }
      action = { type = "expire" }
    }]
  })
}

# Modal imports a role's image from ECR with this role, which it assumes by the identity token
# that Modal's OIDC issuer gives a workspace: AWS takes only a token for the person's workspace,
# as targets' images hold hidden data. The account's OIDC provider is made once (README.md).
data "aws_iam_openid_connect_provider" "modal" {
  url = "https://oidc.modal.com"
}

resource "aws_iam_role" "modal_pull" {
  name        = "${local.prefix}-modal-pull"
  description = "Modal, for one workspace: pull the roles' images"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Federated = data.aws_iam_openid_connect_provider.modal.arn }
      Action    = "sts:AssumeRoleWithWebIdentity"
      Condition = {
        StringEquals = { "oidc.modal.com:aud" = "oidc.modal.com" }
        # Modal's subject begins with the workspace's ID.
        StringLike = { "oidc.modal.com:sub" = "modal:workspace_id:${var.modal_workspace}:*" }
      }
    }]
  })
}

resource "aws_iam_role_policy" "modal_pull" {
  name = "pull"
  role = aws_iam_role.modal_pull.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      { Effect = "Allow", Action = "ecr:GetAuthorizationToken", Resource = "*" },
      {
        Effect   = "Allow"
        Action   = local.ecr_pull
        Resource = aws_ecr_repository.images["roles"].arn
      },
    ]
  })
}

locals {
  # What `docker pull` of an image from ECR takes, once logged in.
  ecr_pull = ["ecr:BatchCheckLayerAvailability", "ecr:BatchGetImage", "ecr:GetDownloadUrlForLayer"]
}
