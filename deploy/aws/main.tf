# One team's Pumpkin on AWS. An admin's root calls this module. README.md says how, and what each
# output feeds.

data "aws_partition" "current" {}

data "aws_caller_identity" "current" {}

# The region of the root's provider, which the data source's `region` defaults to. So the region
# has one home
# (https://github.com/hashicorp/terraform-provider-aws/blob/bc18835a79b092336914f2105a03a200324ea6a9/website/docs/d/region.html.markdown#L31).
data "aws_region" "current" {}

locals {
  prefix  = "pumpkin-${var.name}"
  account = data.aws_caller_identity.current.account_id
  region  = data.aws_region.current.region
  arn     = "arn:${data.aws_partition.current.partition}"
  ec2     = "${local.arn}:ec2:${local.region}:${local.account}"
}
