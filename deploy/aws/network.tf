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

# The controller's `sshd` is reached only from `ssh_from`, and its gateway only from the
# sandboxes. The group's description stays as it is, since
# the provider replaces a group whose description changes, with every rule in it.
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

# Its description has no apostrophe, which EC2 refuses in a rule's description and a plan doesn't
# check.
resource "aws_vpc_security_group_ingress_rule" "controller_ssh" {
  for_each          = toset(var.ssh_from)
  security_group_id = aws_security_group.controller.id
  description       = "ssh from the machines that ssh_from names"
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
