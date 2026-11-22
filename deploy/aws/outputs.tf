output "settings_parameter_arn" {
  description = "The ARN of the SSM parameter of the controller's settings. `pumpkin-admin install --settings` takes it."
  value       = aws_ssm_parameter.settings.arn
}

output "client_config" {
  description = "The `[controller]` table of each member's machine's `pumpkin.toml`, beside its `[upstreams]`."
  value       = "[controller]\nname = ${jsonencode(var.name)}\nregion = ${jsonencode(local.region)}\n"
}

# What a member's machine needs to reach the controller: find it by its tag and start it, then
# reach its `sshd` from `ssh_from` with a key that `members` lists. The machine pushes no key, as
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
