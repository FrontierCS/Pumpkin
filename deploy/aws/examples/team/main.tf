# A team's root, which calls the module. The module's README.md says how to use it. `cargo xtask
# ci` validates this root against the module beside it, as the GitHub runner module validates its
# examples
# (https://github.com/github-aws-runners/terraform-aws-github-runner/blob/f975153d6749b2ea2ddebc8d8cf5911196a1973a/.github/workflows/terraform.yml#L281-L318).

terraform {
  required_version = ">= 1.15.9"
  required_providers {
    aws = { source = "hashicorp/aws", version = "6.72.0" }
  }
  backend "s3" { use_lockfile = true }
}

locals {
  name = "research"
}

provider "aws" {
  region = "us-west-1"
  # Every resource names its team, so that teams' envelopes in one account stay apart.
  default_tags {
    tags = { "pumpkin:envelope" = local.name }
  }
}

module "pumpkin" {
  # A team's own root gives the module's source at a release's tag, as Vault's examples do
  # (https://github.com/hashicorp/terraform-aws-vault/blob/96afcb4ad7166182ef33398d54db5eaa9d1f79c3/examples/vault-cluster-private/main.tf#L18-L19):
  # "git::https://github.com/FrontierCS/Pumpkin.git//deploy/aws?ref=<tag>".
  source = "../.."

  name             = local.name
  vpc_id           = "vpc-…"
  subnet_id        = "subnet-…"
  modal_workspaces = ["ac-alice", "ac-bob"] # alice's and bob's workspaces, who run roles on Modal
  members = {
    alice = ["ssh-ed25519 AAAA… alice-laptop", "ssh-ed25519 AAAA… alice-desk"]
    bob   = ["ssh-ed25519 AAAA… bob"]
  }
  pumpkin_release = var.pumpkin_release
}

# No default, so that `terraform plan` asks for it, while `terraform validate` needs none.
variable "pumpkin_release" {
  type        = string
  description = "The `pumpkinversion` line of the release whose tag the module's `source` names."
}

output "client_config" {
  value = module.pumpkin.client_config
}
