# Terraform 1.11 is the first whose S3 backend locks with an object beside the state
# (`use_lockfile`), so no DynamoDB table is needed. The bucket, the key and the region come from
# `terraform init -backend-config=…`, as README.md says.
terraform {
  required_version = ">= 1.11"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "6.72.0"
    }
  }

  backend "s3" {
    use_lockfile = true
  }
}

provider "aws" {
  region = var.region

  # Every resource names its person, so that teammates' envelopes in one account stay apart.
  default_tags {
    tags = { "pumpkin:envelope" = var.name }
  }
}
