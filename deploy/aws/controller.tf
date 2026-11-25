# The team's controller. It builds the tasks' images and runs the drivers, and it runs no role.
# Its members reach it over ssh from its VPC or from `ssh_from`, each with a key that `members`
# lists. It stops itself when it has no job and no SSH session, by a unit that `pumpkin-admin
# install` sets up, so it costs only its volumes between runs.

resource "aws_iam_role" "controller" {
  name               = "${local.prefix}-controller"
  description        = "Pumpkin's controller: run sandboxes in the envelope, and push their images"
  assume_role_policy = local.ec2_assume_role_policy
}

# What its jobs and its host call on AWS.
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
  modal_settings = flatten([for role in aws_iam_role.modal_pull : [
    "",
    "[providers.modal.modal]",
    "images = { region = ${jsonencode(local.region)}, repository = ${jsonencode(aws_ecr_repository.images["roles"].name)}, role_arn = ${jsonencode(role.arn)} }",
  ]])
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
# the controller's Docker engine, the EC2 provider `aws`, and, with a pull role, the `images` of
# the Modal provider `modal`. A job drops `images` if its client sends no
# `[controllers.<name>.modal]`. With `draining`, each job is refused as it starts. Nothing else is
# here: the rest is each job's own or its client's. The advanced tier holds 8 KB.
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
    local.modal_settings,
    [""],
  ))
}

# The members' keys, a line `<member> <key>` for each key that `members` lists. The controller
# copies them every minute into the file of keys that its `sshd` admits, each with the forced
# command that names its member. `pumpkin-admin install` sets that up. A list of no key is a blank
# line, which the controller skips, as SSM's model gives a value a minimum length of 1
# (https://github.com/boto/botocore/blob/0519e87d43b80dd5cec553728966f99c46d7cbaf/botocore/data/ssm/2014-11-06/service-2.json#L9489-L9493).
resource "aws_ssm_parameter" "members" {
  name  = "/${local.prefix}/members"
  type  = "String"
  tier  = "Advanced"
  value = coalesce(join("", flatten([for member, keys in var.members : [for key in keys : "${member} ${key}\n"]])), "\n")
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

locals {
  controller_user_data_values = {
    install_script  = base64gzip(file("${path.module}/../controller/install.sh"))
    docker_key      = base64gzip(file("${path.module}/../controller/docker.asc"))
    aws_cli_version = local.controller_versions.aws_cli
    aws_cli_sha256  = local.controller_versions.aws_cli_sha256
    uv_version      = local.controller_versions.uv
    uv_sha256       = local.controller_versions.uv_sha256
    pumpkin_release = var.pumpkin_release
    settings_arn    = aws_ssm_parameter.settings.arn
  }
  controller_user_data = templatefile("${path.module}/controller.yaml", merge(
    local.controller_user_data_values, { volume_id = aws_ebs_volume.controller.id }
  ))
  # The same with a volume ID of a volume ID's length, so that a plan measures it before the volume
  # exists: an EBS volume's ID is `vol-` and 17 hexadecimal digits.
  controller_user_data_measured = templatefile("${path.module}/controller.yaml", merge(
    local.controller_user_data_values, { volume_id = "vol-00000000000000000" }
  ))
}

resource "aws_instance" "controller" {
  ami                    = data.aws_ssm_parameter.ubuntu.insecure_value
  instance_type          = var.controller_instance_type
  subnet_id              = var.subnet_id
  vpc_security_group_ids = [aws_security_group.controller.id]
  iam_instance_profile   = aws_iam_instance_profile.controller.name
  # A public address: the way out to the settings parameter, ECR and Modal, and the one Modal's tunnels admit.
  associate_public_ip_address = true
  # A shutdown stops it, so that a member's command can start it again.
  instance_initiated_shutdown_behavior = "stop"
  # What every controller installs is deploy/controller's, beside this module in the package that
  # a root fetches whole
  # (https://github.com/hashicorp/web-unified-docs/blob/4ec2d604280fff09cfca1d2648fed5cb51f81f14/content/terraform/v1.16.x/docs/language/modules/sources.mdx#L484-L487).
  user_data = local.controller_user_data

  # The first boot's install reads the settings and the members' keys.
  depends_on = [aws_iam_role_policy.controller]

  # Tokens required, within one hop, so that no build's container reaches the role.
  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  # The OS and the packages alone, which each instance installs anew: what the controller keeps
  # is on its data volume. 30 GiB, as the GitHub runner module gives a runner's root
  # (https://github.com/github-aws-runners/terraform-aws-github-runner/blob/3355ae029d598d7baf096eebafd19bfd5429bbaa/variables.tf#L324-L340).
  root_block_device {
    volume_size = 30
    volume_type = "gp3"
    encrypted   = true
  }

  # The tag by which `pumpkin` finds the controller of `[controllers.<name>]`.
  tags = { Name = "${local.prefix}-controller", "pumpkin:controller" = var.name }

  # cloud-init runs the user data at the instance's first boot alone. So a new `pumpkin_release`
  # replaces the controller, and the new one's first boot installs it.
  lifecycle {
    # A stopped instance reads as having no public address, and a change to it would replace the
    # instance. The controller stops itself when idle.
    ignore_changes = [ami, user_data, associate_public_ip_address]
    # On the release, not on the user data, so that a change to the rest of the user data replaces
    # nothing.
    replace_triggered_by = [terraform_data.pumpkin_release]
    # EC2 takes 16 KiB of user data, counted before base64 ("User data is limited to 16 KB, in raw
    # form, before it is base64-encoded":
    # https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/user-data.html), and refuses a launch past
    # it after the old instance is gone. Base64 of 16,383 bytes is at most 21,844 characters, which
    # `length` counts where it can't count bytes.
    precondition {
      condition     = length(base64encode(local.controller_user_data_measured)) <= 21844
      error_message = "The controller's user data passes EC2's 16 KiB: shorten deploy/aws/controller.yaml or deploy/controller/."
    }
  }
}

data "aws_subnet" "controller" {
  id = var.subnet_id
}

# What the controller keeps from one instance to the next, as the first boot mounts it: the
# engine's images and build cache, the home of `pumpkin` with its store, trees, jobs, installs and
# records, and the host key. A replacement of the instance moves it to the new one. Its zone stays
# the one it was made in, so that a subnet in another zone fails the attachment instead of
# replacing the volume. GitLab's toolkit keeps a data disk's zone so, and stops its instance before
# it detaches the disk
# (https://gitlab.com/gitlab-org/gitlab-environment-toolkit/-/blob/2eef5ab5ed5a085424096e5ee7e44457390afb04/terraform/modules/gitlab_aws_instance/data_disks.tf#L4-L62).
resource "aws_ebs_volume" "controller" {
  availability_zone = data.aws_subnet.controller.availability_zone
  type              = "gp3"
  size              = var.controller_volume_gib
  iops              = var.controller_volume_iops
  throughput        = var.controller_volume_throughput
  encrypted         = true
  tags              = { Name = "${local.prefix}-controller" }

  lifecycle {
    ignore_changes = [availability_zone]
  }
}

# The instance stops before the volume is detached, so that its shutdown unmounts the volume.
resource "aws_volume_attachment" "controller" {
  device_name                    = "/dev/sdf"
  volume_id                      = aws_ebs_volume.controller.id
  instance_id                    = aws_instance.controller.id
  stop_instance_before_detaching = true
}

# What the controller installs beside what deploy/controller's install.sh pins, at these versions:
# AWS CLI v2 and uv.
locals {
  controller_versions = {
    aws_cli        = "2.38.1"
    aws_cli_sha256 = "b11ed416d91f4251da363a17706938eeaf0e520745754c104c0c1890dd8363c7"
    uv             = "0.13.5"
    uv_sha256      = "f68d291c75783122dad58ab77def0e13f3233f358731db5d6d7e8715603b928f"
  }
}
