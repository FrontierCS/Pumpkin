# The sandboxes dial the gateway on the controller at the port its kernel picks for each run,
# from the ephemeral range, and pull their images from ECR over HTTPS. DNS, time and instance
# metadata go to link-local addresses that security groups don't filter.

# The ephemeral port range of the controller's kernel, Linux's default, where port 0's pick falls.
locals {
  gateway_ports = { from = 32768, to = 60999 }
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

# The controller listens on the Internet for nothing: it is reached through SSM, which its agent
# dials out to, and only the sandboxes reach its gateway.
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

resource "aws_vpc_security_group_egress_rule" "controller_https" {
  security_group_id = aws_security_group.controller.id
  description       = "SSM, ECR, EC2, Modal, model upstreams, and the sources builds fetch"
  ip_protocol       = "tcp"
  from_port         = 443
  to_port           = 443
  cidr_ipv4         = "0.0.0.0/0"
}
