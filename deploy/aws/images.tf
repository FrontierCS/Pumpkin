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
# that Modal's OIDC issuer gives a workspace: AWS takes only a token of a workspace that
# `modal_workspaces` lists, as targets' images hold hidden data. With none, neither the role nor
# the account's OIDC provider is needed. The account's OIDC provider is made once (README.md).
data "aws_iam_openid_connect_provider" "modal" {
  count = length(var.modal_workspaces) > 0 ? 1 : 0
  url   = "https://oidc.modal.com"
}

resource "aws_iam_role" "modal_pull" {
  count       = length(data.aws_iam_openid_connect_provider.modal)
  name        = "${local.prefix}-modal-pull"
  description = "Modal, for the team's workspaces: pull the roles' images"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Federated = data.aws_iam_openid_connect_provider.modal[0].arn }
      Action    = "sts:AssumeRoleWithWebIdentity"
      Condition = {
        StringEquals = { "oidc.modal.com:aud" = "oidc.modal.com" }
        # Modal's subject begins with the workspace's ID, as its guide's trust policy tests it
        # (https://modal.com/docs/guide/oidc-integration). IAM takes a subject that matches any
        # one of the values
        # (https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_policies_condition-logic-multiple-context-keys-or-values.html).
        StringLike = { "oidc.modal.com:sub" = [for workspace in var.modal_workspaces : "modal:workspace_id:${workspace}:*"] }
      }
    }]
  })
}

resource "aws_iam_role_policy" "modal_pull" {
  count = length(aws_iam_role.modal_pull)
  name  = "pull"
  role  = aws_iam_role.modal_pull[0].id
  policy = templatefile("${path.module}/policies/modal-pull.json.tftpl", {
    modal_repository_arn = aws_ecr_repository.images["roles"].arn
  })
}

# A state that a module without `count` here wrote holds each object with no instance key. It
# becomes instance 0, as Terraform's refactoring guide writes it
# (https://github.com/hashicorp/web-unified-docs/blob/4ec2d604280fff09cfca1d2648fed5cb51f81f14/content/terraform/v1.16.x/docs/language/modules/develop/refactoring.mdx#L202-L206).
moved {
  from = aws_iam_role.modal_pull
  to   = aws_iam_role.modal_pull[0]
}

moved {
  from = aws_iam_role_policy.modal_pull
  to   = aws_iam_role_policy.modal_pull[0]
}
