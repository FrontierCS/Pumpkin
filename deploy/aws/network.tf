# The sandboxes dial the gateway on the controller at the port its kernel picks for each run,
# from the ephemeral range, and pull their images from ECR over HTTPS. DNS, time and instance
# metadata go to link-local addresses that security groups don't filter.

# The ephemeral port range of the controller's kernel, Linux's default, where port 0's pick falls.
# `pumpkin`'s tests write the file from `host::GATEWAY_PORTS`, which `pumpkin-admin install`
# requires of the host.
locals {
  gateway_ports = jsondecode(file("${path.module}/gateway_ports.json"))
}

resource "aws_security_group" "sandbox" {
  name        = "${local.prefix}-sandbox"
  description = "Pumpkin sandboxes: no inbound; out to HTTPS and the gateway on the controller"
  vpc_id      = var.vpc_id
}

resource "aws_vpc_security_group_egress_rule" "sandbox_https" {
  security_group_id = aws_security_group.sandbox.id
  description       = "Image pulls from ECR"
  ip_protocol       = "tcp"
  from_port         = 443
  to_port           = 443
  cidr_ipv4         = "0.0.0.0/0"
}

resource "aws_vpc_security_group_egress_rule" "sandbox_gateway" {
  security_group_id            = aws_security_group.sandbox.id
  description                  = "Sessions to the gateway on the controller"
  ip_protocol                  = "tcp"
  from_port                    = local.gateway_ports.from
  to_port                      = local.gateway_ports.to
  referenced_security_group_id = aws_security_group.controller.id
}

# The controller's `sshd` is reached only from its VPC, and its gateway only from the sandboxes.
# The group's description stays as it is, since the provider replaces a group whose description
# changes, with every rule in it.
resource "aws_security_group" "controller" {
  name        = "${local.prefix}-controller"
  description = "Pumpkin controller: in from its sandboxes alone; out to HTTPS"
  vpc_id      = var.vpc_id
}

resource "aws_vpc_security_group_ingress_rule" "controller_gateway" {
  security_group_id            = aws_security_group.controller.id
  description                  = "Sessions from the sandboxes"
  ip_protocol                  = "tcp"
  from_port                    = local.gateway_ports.from
  to_port                      = local.gateway_ports.to
  referenced_security_group_id = aws_security_group.sandbox.id
}

data "aws_vpc" "this" {
  id = var.vpc_id
}

# Each IPv4 block associated with the VPC reaches port 22. rust-lang's EFS module admits its VPC's
# primary block, read the same way
# (https://github.com/rust-lang/simpleinfra/blob/75291a2cdf0dd5ba60860701274f9500703102da/terraform/shared/modules/efs-filesystem/main.tf#L23-L48).
# A block that is still being associated gets its rule at the next apply. `sshd` admits for
# `pumpkin` only the keys that `members` lists, and for `ubuntu` only a key that EC2 Instance
# Connect pushes for an IAM principal.
# Its description has no apostrophe, which EC2 refuses in a rule's description and a plan doesn't
# check.
resource "aws_vpc_security_group_ingress_rule" "controller_ssh" {
  for_each          = toset([for block in data.aws_vpc.this.cidr_block_associations : block.cidr_block if block.state == "associated"])
  security_group_id = aws_security_group.controller.id
  description       = "ssh from the VPC"
  ip_protocol       = "tcp"
  from_port         = 22
  to_port           = 22
  cidr_ipv4         = each.value
}

resource "aws_vpc_security_group_egress_rule" "controller_https" {
  security_group_id = aws_security_group.controller.id
  description       = "SSM, ECR, EC2, Modal, model upstreams, and the sources builds fetch"
  ip_protocol       = "tcp"
  from_port         = 443
  to_port           = 443
  cidr_ipv4         = "0.0.0.0/0"
}
