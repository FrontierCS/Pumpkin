# The AMI that the sandboxes boot, baked once by EC2 Image Builder: Amazon Linux 2023 at a named
# release, with Docker, ECR's credential helper and nftables, which each sandbox's boot runs. The
# AMI installs the helper without configuring it. Each boot's user data configures it. AL2023's
# package repositories are fixed to the release of the AMI that they run on, so the release pins
# the version of every package installed here. A change to `packages` or `release` also bumps
# `version`, which names the component and the recipe, and bakes a new AMI.
locals {
  ami_build = {
    release  = "2023.12.20261102.0"
    packages = ["amazon-ecr-credential-helper", "docker", "nftables"]
    version  = "1.1.0"
  }
}

data "aws_ami" "al2023" {
  owners = ["amazon"]

  filter {
    name   = "name"
    values = ["al2023-ami-${local.ami_build.release}-kernel-6.12-x86_64"]
  }
}

resource "aws_imagebuilder_component" "packages" {
  name     = "${local.prefix}-packages"
  version  = local.ami_build.version
  platform = "Linux"
  data = yamlencode({
    schemaVersion = 1.0
    phases = [{
      name = "build"
      steps = [{
        name   = "install"
        action = "ExecuteBash"
        inputs = { commands = ["dnf install --assumeyes ${join(" ", local.ami_build.packages)}"] }
      }]
    }]
  })
}

resource "aws_imagebuilder_image_recipe" "pumpkin" {
  name         = local.prefix
  version      = local.ami_build.version
  parent_image = data.aws_ami.al2023.id

  component {
    component_arn = aws_imagebuilder_component.packages.arn
  }
}

# The build instance: Image Builder drives it through SSM, and it reaches the release's
# repositories over HTTPS, as a sandbox reaches ECR.
resource "aws_iam_role" "image_builder" {
  name               = "${local.prefix}-image-builder"
  description        = "Pumpkin's AMI build instance"
  assume_role_policy = local.ec2_assume_role_policy
}

resource "aws_iam_role_policy_attachment" "image_builder" {
  for_each = toset(["EC2InstanceProfileForImageBuilder", "AmazonSSMManagedInstanceCore"])

  role       = aws_iam_role.image_builder.name
  policy_arn = "${local.arn}:iam::aws:policy/${each.key}"
}

resource "aws_iam_instance_profile" "image_builder" {
  name = "${local.prefix}-image-builder"
  role = aws_iam_role.image_builder.name
}

resource "aws_imagebuilder_infrastructure_configuration" "pumpkin" {
  name                          = local.prefix
  instance_profile_name         = aws_iam_instance_profile.image_builder.name
  instance_types                = ["c6a.large"]
  subnet_id                     = var.subnet_id
  security_group_ids            = [aws_security_group.sandbox.id]
  terminate_instance_on_failure = true

  # Tokens required, within one hop, as Image Builder's security best practices ask.
  instance_metadata_options {
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }
}

# A new image leaves the old AMI registered, as README.md's teardown says.
resource "aws_imagebuilder_image" "pumpkin" {
  image_recipe_arn                 = aws_imagebuilder_image_recipe.pumpkin.arn
  infrastructure_configuration_arn = aws_imagebuilder_infrastructure_configuration.pumpkin.arn

  # The recipe has no test component, so a test phase would only boot the AMI once more.
  image_tests_configuration {
    image_tests_enabled = false
  }
}

locals {
  ami = one(aws_imagebuilder_image.pumpkin.output_resources[0].amis).image
}
