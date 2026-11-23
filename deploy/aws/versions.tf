# A child module. The root that calls it holds the provider's configuration, with its region and
# `default_tags`, and the backend and the lock file, as README.md says. A module with a provider
# block of its own can't be called with `count`, `for_each` or `depends_on`
# (https://github.com/hashicorp/web-unified-docs/blob/4ec2d604280fff09cfca1d2648fed5cb51f81f14/content/terraform/v1.16.x/docs/language/modules/develop/providers.mdx#L29-L33).
# So this module declares only what it requires, as the GitHub runner module does
# (https://github.com/github-aws-runners/terraform-aws-github-runner/blob/f975153d6749b2ea2ddebc8d8cf5911196a1973a/versions.tf#L1-L14).
# `cargo xtask ci` validates it with Terraform 1.15.9 and the provider 6.72.0, so those are the
# oldest versions it allows. The root pins the provider at an exact version.
terraform {
  required_version = ">= 1.15.9"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 6.72"
    }
  }
}
