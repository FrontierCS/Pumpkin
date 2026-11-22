output "settings_parameter_arn" {
  description = "The ARN of the SSM parameter of the controller's settings. `pumpkin-admin install --settings` takes it."
  value       = aws_ssm_parameter.settings.arn
}

output "client_config" {
  description = "The `[controller]` table of each member's machine's `pumpkin.toml`, beside its `[upstreams]`."
  value       = "[controller]\nname = ${jsonencode(var.name)}\nregion = ${jsonencode(local.region)}\n"
}

# What a member's machine needs to reach the controller: find it by its tag and start it, then
# reach its `sshd` from its VPC, or through the endpoint, with a key that `members` lists. The
# machine pushes no key, as the admin's list is the only way in.
resource "aws_iam_policy" "client" {
  name        = "${local.prefix}-client"
  description = "What the machine of a person needs to reach its Pumpkin controller"
  policy = templatefile("${path.module}/policies/client.json.tftpl", {
    controller_instance_arn       = "${local.ec2}:instance/${aws_instance.controller.id}"
    controller_private_ip         = aws_instance.controller.private_ip
    instance_connect_endpoint_arn = aws_ec2_instance_connect_endpoint.controller.arn
  })
}

output "client_policy_arn" {
  description = "The managed IAM policy that each member's machine needs to reach the controller, which the team's root or an administrator attaches to the machine's role or user."
  value       = aws_iam_policy.client.arn
}

# What a contributor's machine needs for `cargo xtask live`: the client's, what `pumpkin-admin
# check` reads, and the list of the timer's schedules that a run left. The team's root attaches
# it to the contributors.
resource "aws_iam_policy" "contributor" {
  name        = "${local.prefix}-contributor"
  description = "What the machine of a contributor needs to run the live checks on its Pumpkin controller"
  policy = templatefile("${path.module}/policies/contributor.json.tftpl", {
    controller_instance_arn       = "${local.ec2}:instance/${aws_instance.controller.id}"
    controller_private_ip         = aws_instance.controller.private_ip
    instance_connect_endpoint_arn = aws_ec2_instance_connect_endpoint.controller.arn
    members_parameter_arn         = aws_ssm_parameter.members.arn
    sandbox_profile_arn           = aws_iam_instance_profile.sandbox.arn
    settings_parameter_arn        = aws_ssm_parameter.settings.arn
  })
}

output "contributor_policy_arn" {
  description = "The managed IAM policy that each contributor's machine needs for `cargo xtask live`, which the team's root attaches."
  value       = aws_iam_policy.contributor.arn
}
