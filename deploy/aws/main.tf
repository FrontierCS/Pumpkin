# One team's Pumpkin on AWS. An admin's root calls this module. README.md says how, and what each
# output feeds.
#
# Each principal's permission policy is `policies/<principal>.json.tftpl`, which `pumpkin-admin
# policy <principal>` prints from what the code's calls on AWS need. So a change to a call is a
# change to the code that makes it, never to a policy file: `cargo xtask refill --package pumpkin
# policy` writes the files again. `templatefile` fills each, as the GitHub runner module fills its
# policies
# (https://github.com/github-aws-runners/terraform-aws-github-runner/blob/f975153d6749b2ea2ddebc8d8cf5911196a1973a/modules/runners/policies-runner.tf#L31-L36).
# The files are committed, as Kubebuilder's projects commit the RBAC that `controller-gen` writes
# from the code
# (https://github.com/kubernetes-sigs/kubebuilder/blob/4d01fcdf146e4d832fc973ef95684a00bcf0f0c0/pkg/plugins/golang/v4/scaffolds/internal/templates/makefile.go#L121-L123).
# No call states a trust policy, so each is the module's own, as are the AWS-managed policies of
# the AMI's build, under which Pumpkin makes no call.

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

  # The trust policy of each role that EC2's instances assume.
  ec2_assume_role_policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Principal = { Service = "ec2.amazonaws.com" }, Action = "sts:AssumeRole" }]
  })
}
