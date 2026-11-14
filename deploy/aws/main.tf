# One person's Pumpkin on AWS: the EC2 envelope that the `ec2` provider launches sandboxes in
# (sandboxes.tf), the image repositories and Modal's pull role (images.tf), the AMI that the
# sandboxes boot (ami.tf), the controller and its jobs' settings (controller.tf) and the
# security groups (network.tf). README.md says how to apply it, and what each output feeds.

data "aws_partition" "current" {}

data "aws_caller_identity" "current" {}

locals {
  prefix  = "pumpkin-${var.name}"
  account = data.aws_caller_identity.current.account_id
  arn     = "arn:${data.aws_partition.current.partition}"
  ec2     = "${local.arn}:ec2:${var.region}:${local.account}"
}
