# The EC2 envelope that the `ec2` provider's module doc, "The envelope", relies on: what each
# sandbox's instance runs as, a launch template for each class, and the timer that terminates an
# instance at its run's hard end.

# A sandbox's instance pulls its role's image and the `sandboxd` image, and may do nothing else.
resource "aws_iam_role" "sandbox" {
  name        = "${local.prefix}-sandbox"
  description = "Pumpkin sandbox instances: pull their images"
  assume_role_policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Principal = { Service = "ec2.amazonaws.com" }, Action = "sts:AssumeRole" }]
  })
}

resource "aws_iam_role_policy" "sandbox" {
  name = "pull"
  role = aws_iam_role.sandbox.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      { Effect = "Allow", Action = "ecr:GetAuthorizationToken", Resource = "*" },
      {
        Effect   = "Allow"
        Action   = local.ecr_pull
        Resource = [for repository in aws_ecr_repository.images : repository.arn]
      },
    ]
  })
}

resource "aws_iam_instance_profile" "sandbox" {
  name = "${local.prefix}-sandbox"
  role = aws_iam_role.sandbox.name
}

# Each class's instance type, for the CPUs and memory that the settings state.
data "aws_ec2_instance_type" "class" {
  for_each = var.classes

  instance_type = each.value.instance_type
}

resource "aws_launch_template" "class" {
  for_each = var.classes

  name          = "${local.prefix}-${each.key}"
  description   = "Pumpkin sandboxes of class ${each.key}"
  image_id      = local.ami
  instance_type = each.value.instance_type

  iam_instance_profile {
    arn = aws_iam_instance_profile.sandbox.arn
  }

  # Tokens required, within one hop, so a container behind the instance's bridge gets none.
  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  # A public address, through which pulls reach ECR over the internet gateway.
  network_interfaces {
    device_index                = 0
    subnet_id                   = var.subnet_id
    security_groups             = [aws_security_group.sandbox.id]
    associate_public_ip_address = true
    delete_on_termination       = true
  }

  block_device_mappings {
    device_name = data.aws_ami.al2023.root_device_name

    ebs {
      volume_size           = var.root_volume_gib
      volume_type           = "gp3"
      encrypted             = true
      delete_on_termination = true
    }
  }
}

# The timer outside each instance's guest: the controller makes a one-shot schedule in this
# group for each instance at its run's hard end, which terminates it with this role.
resource "aws_scheduler_schedule_group" "timer" {
  name = local.prefix
}

resource "aws_iam_role" "timer" {
  name        = "${local.prefix}-timer"
  description = "Pumpkin's timer: terminate a sandbox instance at its run's hard end"
  # Scheduler assumes it only for this group's schedules, as its confused deputy guide asks.
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "scheduler.amazonaws.com" }
      Action    = "sts:AssumeRole"
      Condition = {
        StringEquals = {
          "aws:SourceAccount" = local.account
          "aws:SourceArn"     = aws_scheduler_schedule_group.timer.arn
        }
      }
    }]
  })
}

resource "aws_iam_role_policy" "timer" {
  name   = "terminate"
  role   = aws_iam_role.timer.id
  policy = jsonencode({ Version = "2012-10-17", Statement = [local.terminate_sandboxes] })
}

locals {
  # Terminate an instance that a run launched, as its `pumpkin:run` tag says.
  terminate_sandboxes = {
    Effect    = "Allow"
    Action    = "ec2:TerminateInstances"
    Resource  = "${local.ec2}:instance/*"
    Condition = { Null = { "ec2:ResourceTag/pumpkin:run" = "false" } }
  }
}
