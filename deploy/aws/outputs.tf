output "settings_parameter_arn" {
  description = "The ARN of the SSM parameter of the controller's settings. `pumpkin-admin install --settings` takes it."
  value       = aws_ssm_parameter.settings.arn
}

output "client_config" {
  description = "The `[controller]` table of each member's machine's `pumpkin.toml`, beside its `[upstreams]`."
  value       = "[controller]\nname = ${jsonencode(var.name)}\nregion = ${jsonencode(local.region)}\n"
}

# What a member's machine needs to reach the controller: find it by its tag and start it, then
# reach its `sshd` from its VPC with a key that `members` lists. The machine pushes no key, as
# the admin's list is the only way in.
resource "aws_iam_policy" "client" {
  name        = "${local.prefix}-client"
  description = "What the machine of a person needs to reach its Pumpkin controller"
  policy = templatefile("${path.module}/policies/client.json.tftpl", {
    controller_instance_arn = "${local.ec2}:instance/${aws_instance.controller.id}"
  })
}

output "client_policy_arn" {
  description = "The managed IAM policy that each member's machine needs to reach the controller, which the team's root or an administrator attaches to the machine's role or user."
  value       = aws_iam_policy.client.arn
}

# The instance profile of a member's machine in the VPC, whose role holds the client's policy. A
# machine that runs with it needs only its key in `members`. `cargo xtask live join` launches
# a machine with it.
resource "aws_iam_role" "member" {
  name        = "${local.prefix}-member"
  description = "A member's machine, which runs pumpkin against the team's controller"
  assume_role_policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Principal = { Service = "ec2.amazonaws.com" }, Action = "sts:AssumeRole" }]
  })
}

resource "aws_iam_role_policy_attachment" "member_client" {
  role       = aws_iam_role.member.name
  policy_arn = aws_iam_policy.client.arn
}

resource "aws_iam_instance_profile" "member" {
  name = "${local.prefix}-member"
  role = aws_iam_role.member.name
}

output "member_profile_name" {
  description = "The instance profile of a member's machine in the controller's VPC, which holds the client's policy."
  value       = aws_iam_instance_profile.member.name
}

# What a contributor's machine needs for `cargo xtask live`: the client's, what `pumpkin-admin
# check` reads, the list of the timer's schedules that a run left, and the launch and the end of
# a member's machine. The team's root attaches it to the contributors.
resource "aws_iam_policy" "contributor" {
  name        = "${local.prefix}-contributor"
  description = "What the machine of a contributor needs to run the live checks on its Pumpkin controller"
  policy = templatefile("${path.module}/policies/contributor.json.tftpl", {
    partition               = data.aws_partition.current.partition
    region                  = local.region
    account                 = local.account
    controller_instance_arn = "${local.ec2}:instance/${aws_instance.controller.id}"
    member_profile_arn      = aws_iam_instance_profile.member.arn
    member_role_arn         = aws_iam_role.member.arn
    members_parameter_arn   = aws_ssm_parameter.members.arn
    sandbox_profile_arn     = aws_iam_instance_profile.sandbox.arn
    settings_parameter_arn  = aws_ssm_parameter.settings.arn
  })
}

output "contributor_policy_arn" {
  description = "The managed IAM policy that each contributor's machine needs for `cargo xtask live`, which the team's root attaches."
  value       = aws_iam_policy.contributor.arn
}
