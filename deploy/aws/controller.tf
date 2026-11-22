# The team's controller. It builds the tasks' images and runs the drivers, and it runs no role.
# Its members reach it over ssh from its VPC, each with a key that `members` lists. It stops
# itself when idle, by a unit that `pumpkin-admin install` sets up, so it costs only its volume
# between runs.

resource "aws_iam_role" "controller" {
  name        = "${local.prefix}-controller"
  description = "Pumpkin's controller: run sandboxes in the envelope, and push their images"
  assume_role_policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Principal = { Service = "ec2.amazonaws.com" }, Action = "sts:AssumeRole" }]
  })
}

# What its jobs and its host call on AWS, as README.md's "Policies" says.
resource "aws_iam_role_policy" "controller" {
  name = "run"
  role = aws_iam_role.controller.id
  policy = templatefile("${path.module}/policies/controller.json.tftpl", {
    partition               = data.aws_partition.current.partition
    region                  = local.region
    account                 = local.account
    launch_template_arns    = [for template in aws_launch_template.class : template.arn]
    instance_types          = sort(distinct([for template in aws_launch_template.class : template.instance_type]))
    members_parameter_arn   = aws_ssm_parameter.members.arn
    settings_parameter_arn  = aws_ssm_parameter.settings.arn
    host_key_parameter_arn  = aws_ssm_parameter.host_key.arn
    modal_repository_arn    = aws_ecr_repository.images["roles"].arn
    roles_repository_arn    = aws_ecr_repository.images["roles"].arn
    sandboxd_repository_arn = aws_ecr_repository.images["sandboxd"].arn
    sandbox_profile_arn     = aws_iam_instance_profile.sandbox.arn
    sandbox_role_arn        = aws_iam_role.sandbox.arn
    timer_group             = aws_scheduler_schedule_group.timer.name
    timer_role_arn          = aws_iam_role.timer.arn
  })
}

resource "aws_iam_instance_profile" "controller" {
  name = "${local.prefix}-controller"
  role = aws_iam_role.controller.name
}

# Pumpkin's settings as TOML lines of dotted keys, which hold under any table's header. TOML's
# basic strings and arrays take JSON's, so `jsonencode` writes each value.
locals {
  modal_images = "images = { region = ${jsonencode(local.region)}, repository = ${jsonencode(aws_ecr_repository.images["roles"].name)}, role_arn = ${jsonencode(aws_iam_role.modal_pull.arn)} }"
  ec2_settings = concat(
    [
      "region = ${jsonencode(local.region)}",
      "repositories = { roles = ${jsonencode(aws_ecr_repository.images["roles"].name)}, sandboxd = ${jsonencode(aws_ecr_repository.images["sandboxd"].name)} }",
      "timer = { group = ${jsonencode(aws_scheduler_schedule_group.timer.name)}, role_arn = ${jsonencode(aws_iam_role.timer.arn)} }",
    ],
    [for class, template in aws_launch_template.class : join("", [
      "classes.${class} = { template = ${jsonencode(template.id)}, version = ${template.latest_version}",
      ", vcpus = ${data.aws_ec2_instance_type.class[class].default_vcpus}",
      ", memory_mib = ${data.aws_ec2_instance_type.class[class].memory_size}",
      ", cpu_features = ${jsonencode(sort(var.classes[class].cpu_features))}",
      var.classes[class].cpu_model == null ? "" : ", cpu_model = ${jsonencode(var.classes[class].cpu_model)}",
      var.classes[class].usd_per_hour == null ? "" : ", usd_per_hour = ${jsonencode(var.classes[class].usd_per_hour)}",
      " }",
    ])],
  )
}

# Each job's settings. `pumpkin controller job` reads them as it starts, so an apply that changes
# them takes effect at the next job. Their `[deployment]` names the deployment and the members'
# parameter. The host's members unit reads it every minute, and each job leaves it out. They name
# the controller's Docker engine, the EC2 provider `aws`, and the `images` of the Modal provider
# `modal`. A job drops `images` if its client sends no `[controller.modal]`. With `draining`, each
# job is refused as it starts. Nothing else is here: the rest is each job's own or its client's.
# The advanced tier holds 8 KB.
resource "aws_ssm_parameter" "settings" {
  name = "/${local.prefix}/settings"
  type = "String"
  tier = "Advanced"
  value = join("\n", concat(
    # Keys of the top, before any table. Envelope 1.1 adds `[deployment]`. The host reads it, and
    # each job leaves it out.
    ["envelope = \"1.1\""],
    var.draining ? ["draining = true"] : [],
    [
      "",
      "[deployment]",
      "name = ${jsonencode(var.name)}",
      "members = ${jsonencode(aws_ssm_parameter.members.arn)}",
      "",
      "[docker]", "context = \"default\"", "", "[providers.aws.ec2]",
    ],
    local.ec2_settings,
    ["", "[providers.modal.modal]", local.modal_images],
    [""],
  ))
}

# The members' keys, a line `<member> <key>` for each key that `members` lists. The controller
# copies them every minute into the file of keys that its `sshd` admits, each with the forced
# command that names its member. `pumpkin-admin install` sets that up.
resource "aws_ssm_parameter" "members" {
  name  = "/${local.prefix}/members"
  type  = "String"
  tier  = "Advanced"
  value = join("", flatten([for member, keys in var.members : [for key in keys : "${member} ${key}\n"]]))
}

# The controller's ed25519 host key, as `ssh-ed25519 <blob>`. Its first boot's `pumpkin-admin
# install` publishes it here, and each member's machine pins the key it reads here. Until then
# it holds no key, and a member's machine pins the key that the controller shows first. The
# controller writes it, so an apply leaves its value as it is.
resource "aws_ssm_parameter" "host_key" {
  name  = "/${local.prefix}/host-key"
  type  = "String"
  value = "unpublished"
  lifecycle {
    ignore_changes = [value]
  }
}

# Canonical's Ubuntu 24.04 LTS, as its public parameter names the current release, whose Docker
# comes from Docker's own repository, as builds need Docker 28 or later with the containerd
# image store, and Buildx. A controller takes the Ubuntu release that is current when it is made.
data "aws_ssm_parameter" "ubuntu" {
  name = "/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id"
}

# The release that the controller's first boot installs. `replace_triggered_by` takes only a
# resource's change, so this holds the variable, and a change to it replaces the controller
# (https://github.com/hashicorp/web-unified-docs/blob/4ec2d604280fff09cfca1d2648fed5cb51f81f14/content/terraform/v1.16.x/docs/language/meta-arguments/lifecycle.mdx#L112-L145).
resource "terraform_data" "pumpkin_release" {
  input = var.pumpkin_release
}

resource "aws_instance" "controller" {
  ami                    = data.aws_ssm_parameter.ubuntu.insecure_value
  instance_type          = var.controller_instance_type
  subnet_id              = var.subnet_id
  vpc_security_group_ids = [aws_security_group.controller.id]
  iam_instance_profile   = aws_iam_instance_profile.controller.name
  # A public address: the way out to the settings parameter, ECR and Modal, and the one Modal's tunnels admit.
  associate_public_ip_address = true
  # A shutdown stops it, keeping its volume: the build cache, the trees and the records.
  instance_initiated_shutdown_behavior = "stop"
  user_data = templatefile("${path.module}/controller.yaml", {
    ubuntu_snapshot    = "https://snapshot.ubuntu.com/ubuntu/${local.controller_versions.ubuntu_snapshot}"
    docker_key         = file("${path.module}/docker.asc")
    docker_version     = local.controller_versions.docker
    containerd_version = local.controller_versions.containerd
    buildx_version     = local.controller_versions.buildx
    aws_cli_version    = local.controller_versions.aws_cli
    aws_cli_sha256     = local.controller_versions.aws_cli_sha256
    uv_version         = local.controller_versions.uv
    uv_sha256          = local.controller_versions.uv_sha256
    pumpkin_release    = var.pumpkin_release
    settings_arn       = aws_ssm_parameter.settings.arn
  })

  # The first boot's install reads the settings and the members' keys, and publishes the host key.
  depends_on = [aws_iam_role_policy.controller]

  # Tokens required, within one hop, so that no build's container reaches the role.
  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  root_block_device {
    volume_size = var.controller_volume_gib
    volume_type = "gp3"
    iops        = var.controller_volume_iops
    throughput  = var.controller_volume_throughput
    encrypted   = true
  }

  # The tag by which `pumpkin` finds the controller of `[controller] name`.
  tags = { Name = "${local.prefix}-controller", "pumpkin:controller" = var.name }

  # cloud-init runs the user data at the instance's first boot alone. So a new `pumpkin_release`
  # replaces the controller, and the new one's first boot installs it.
  lifecycle {
    # A stopped instance reads as having no public address, and a change to it replaces the
    # instance, its volume and the records on it with it. The controller stops itself when idle.
    ignore_changes = [ami, user_data, associate_public_ip_address]
    # On the release, not on the user data, so that a change to the rest of the user data replaces
    # nothing.
    replace_triggered_by = [terraform_data.pumpkin_release]
  }
}

# What the controller installs, at these versions: Docker's packages, as its repository names
# them; AWS CLI v2; uv; and the rest from Ubuntu's archive as it was at the snapshot.
locals {
  controller_versions = {
    ubuntu_snapshot = "20261101T000000Z"
    docker          = "5:29.9.2-1~ubuntu.24.04~noble"
    containerd      = "2.4.2-3~ubuntu.24.04~noble"
    buildx          = "0.38.1-1~ubuntu.24.04~noble"
    aws_cli         = "2.38.1"
    aws_cli_sha256  = "b11ed416d91f4251da363a17706938eeaf0e520745754c104c0c1890dd8363c7"
    uv              = "0.13.5"
    uv_sha256       = "f68d291c75783122dad58ab77def0e13f3233f358731db5d6d7e8715603b928f"
  }
}
