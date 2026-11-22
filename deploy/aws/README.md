# deploy/aws

The Terraform module that makes one team's Pumpkin on AWS. The team's admin applies it with
administrator credentials, and lists the members by key. Every name it makes begins
`pumpkin-<name>`, where `<name>` is the team's name, so teams share an account without colliding.
Its state is in an S3 bucket of the account, so no laptop can lose it.

| What | Why |
| --- | --- |
| the sandboxes' security group: out to HTTPS and to the controller's ephemeral ports; nothing in | sandboxes pull from ECR and dial the gateway at port 0's pick, and reach nothing else |
| the controller's security group: in from the sandboxes on the ephemeral ports, and from `ssh_from`, none by default, on port 22; out to HTTPS | the gateway, and `sshd` for the members' machines alone |
| ECR `pumpkin-<name>/roles` and `/sandboxd`, immutable tags, untagged images expire after 14 days | private images, pushed tagged by digest |
| a launch template per class: the AMI, the type, a root volume deleted with the instance | the EC2 classes, and what the `ec2` provider relies on; the provider's launch sets the metadata options |
| the sandboxes' role: ECR pulls only | each sandbox's pulls |
| the timer: a schedule group and a role that terminates the run instances of this envelope's sandbox profile | a kill bound outside each guest |
| the Modal pull role: ECR pulls from `roles`, assumed only by `modal_workspace`'s tokens | Modal imports the images of calls' targets, which hold hidden data |
| an AMI baked by EC2 Image Builder from a named AL2023 release, with Docker, ECR's credential helper and nftables | each sandbox's boot runs them, at the release's versions |
| the controller's role and instance profile: access to its jobs' settings parameter, the members' parameter and the EC2 provider's resources: launches from its templates with nothing they name overridden and at their instance types, and terminations and console reads of its own sandboxes alone | the runs' launches, timers, sweeps and pushes, from the controller |
| the controller: an on-demand instance on Ubuntu 24.04 LTS, with a public IPv4 address, IMDSv2 within one hop, an encrypted gp3 root, stopping on shutdown, tagged `pumpkin:controller=<name>`; an apply never replaces it | builds the tasks' images and runs the drivers, which a person's machine sends it, and runs no role |
| what cloud-init installs there: Docker, which runs as root, so its `docker` group, which holds `pumpkin` alone, is root-equivalent; Buildx, AWS CLI v2 and uv at exact versions, and the rest of `controller.yaml`'s packages from a snapshot of Ubuntu's archive | builds with the containerd image store |
| what the Pumpkin release `pumpkin_release` installs there, with `pumpkin-admin install` (`pumpkin-admin install --help` says what): the user `pumpkin`, lingering; an `sshd` that admits concurrent openings and ends a session whose client has gone; a unit that stops the controller after 30 minutes with no job and no SSH session; and one that runs each installed build's reap at boot | jobs that outlive every connection; a stop and a reap that need no client |
| the SSM parameter `/pumpkin-<name>/settings` (advanced tier), of envelope 1.1: the `[deployment]` that the host reads, with the deployment's `name` and members' parameter; the Docker engine, the EC2 provider `aws`, and the Modal provider `modal`'s `images` | each job's settings, which an apply changes for the next job; the rest of the Modal provider is the client's |
| the SSM parameter `/pumpkin-<name>/members` (advanced tier): a line `<member> <key>` for each key in `members`; the controller copies it every minute into `/etc/ssh/pumpkin/members`, with a forced command that names each key's member; `sshd` admits for `pumpkin` only the keys in that file | who may reach the controller, and which member each job belongs to; an apply changes the list within a minute |

`ami.tf`, `controller.tf` and the other files say more of each. What the `ec2` provider requires
of its envelope is that crate's module doc, "The envelope".

## Once per account

The state bucket, versioned so a bad write can be undone, and, if `aws iam
list-open-id-connect-providers` lists no `oidc.modal.com`, Modal's OIDC provider:

```sh
aws s3api create-bucket --bucket pumpkin-tfstate-<account>-<region> --region <region> \
    --create-bucket-configuration LocationConstraint=<region>
aws s3api put-bucket-versioning --bucket pumpkin-tfstate-<account>-<region> \
    --versioning-configuration Status=Enabled
aws iam create-open-id-connect-provider --url https://oidc.modal.com --client-id-list oidc.modal.com
```

## Per team

Terraform 1.11 or later, and the AWS CLI with administrator credentials for the account. The
module is this directory. A root of the admin's own calls it, pinned by a Pumpkin release's tag, so
a release's `pumpkin-admin install` and its module agree. The root holds what is the deployment's:
the provider's configuration with its region and tags, the backend, and the lock file that
`terraform init` writes. The provider's region is the module's. In a directory of its own, such as
a repository, write `main.tf` (`variables.tf` says each of the module's values and its default):

```hcl
terraform {
  required_version = ">= 1.11"
  required_providers {
    aws = { source = "hashicorp/aws", version = "6.72.0" }
  }
  backend "s3" { use_lockfile = true }
}

locals {
  name = "frontier"
}

provider "aws" {
  region = "us-west-1"
  # Every resource names its team, so that teams' envelopes in one account stay apart.
  default_tags {
    tags = { "pumpkin:envelope" = local.name }
  }
}

module "pumpkin" {
  source = "git::https://github.com/FrontierCS/Pumpkin.git//deploy/aws?ref=<tag>"

  name            = local.name
  vpc_id          = "vpc-…"
  subnet_id       = "subnet-…"
  modal_workspace = "ac-…"
  ssh_from        = ["10.0.1.23/32", "10.0.1.24/32"] # the members' machines' private addresses
  members = {
    alice = ["ssh-ed25519 AAAA… alice-laptop", "ssh-ed25519 AAAA… alice-desk"]
    bob   = ["ssh-ed25519 AAAA… bob"]
  }
  pumpkin_release = "<tag> <commit> <sha256>" # the same release's `pumpkinversion` line
}

output "client_config" {
  value = module.pumpkin.client_config
}
```

Give an `output` likewise for each other output that "What the outputs feed" or "Versions" names
and the admin reads. `pumpkin` reaches the controller at its private address, so each member's
machine is in the controller's VPC, or routed to it, and `ssh_from` admits it. A team of one lists
its own key.

Then, in that directory:

```sh
terraform init -backend-config=bucket=pumpkin-tfstate-<account>-<region> \
    -backend-config=key=<name>/terraform.tfstate -backend-config=region=<region>
terraform plan -out=pumpkin.tfplan
terraform apply pumpkin.tfplan
```

The first apply waits while Image Builder bakes the AMI on a build instance of its own. It makes
the controller after the bake, as the controller's settings name the templates that boot the AMI. A
concurrent `plan` or `apply` of the same state waits on its lock. To take a later release's module,
set a new `ref`, run `terraform init -upgrade`, and plan. "Versions" says what a release may change.

A state that an apply made with this directory as its root moves into the module once. The root
gets a `moved` block for each of the module's resources, as Terraform's refactoring guide moves
resources into a module
([refactoring.mdx](https://github.com/hashicorp/web-unified-docs/blob/4ec2d604280fff09cfca1d2648fed5cb51f81f14/content/terraform/v1.16.x/docs/language/modules/develop/refactoring.mdx#L371-L409)):

```hcl
moved {
  from = aws_instance.controller
  to   = module.pumpkin.aws_instance.controller
}
```

`grep '^resource' *.tf` here lists them. Besides the moves, the plan updates the controller's
`run` policy, the sandbox's and the client's to this release's, and destroys the controller's
`settings` and `members` policies, which `run` now holds. The update and the destroys are
unordered, so apply it as a cutover, with the drain runbook under "Versions". Once it is applied,
the blocks can go.

## Members

The controller admits a key only if `members` lists it. It names each job by the member whose key
started it. `ls` and `view` show each run's member. `cancel` stops only the member's own runs, unless
given `--any-member`. This prevents accidents, not members.
Each member's build runs as the controller's one user, `pumpkin`, whose `docker` group is root
there.

- **Enrol.** A machine's first `pumpkin` command makes its key. The controller refuses it, and the
  command prints the line to send the admin. That line is the key's
  `~/.config/pumpkin/ssh/key.pub`, which holds no secret. Add it under the member's name, with a
  list item for each of their machines, and apply. The controller refuses the whole list if a
  name isn't 1 to 32 of a-z, 0-9 and `-` starting with a letter, if a line isn't an `ssh-ed25519`
  key with an optional comment, or if a key is listed twice. It keeps the last list it took. Its
  journal says why: `journalctl -u pumpkin-members.service`.
- **Revoke.** Remove the line and apply. The controller refuses the key within a minute. A
  connection already open lasts until it ends. A member was root on the controller through
  `docker`, so replace the controller to be sure they left nothing behind. `serve` ends a link that asks nothing for a minute,
  but an open `view` page asks every few seconds until it is closed.
- **See the logins.** On the controller,
  `sudo journalctl -u ssh --grep 'Accepted publickey for pumpkin'` lists each login with its
  key's fingerprint. `ssh-keygen -l -f /etc/ssh/pumpkin/members` lists the fingerprint of each key
  with its member's name.

The install copies the list first. It fails, and changes nothing in `sshd`, if the copy fails.
Then `sshd` admits only the listed keys. After an install, check that
`ssh-keygen -l -f /etc/ssh/pumpkin/members` names each member's keys, and that `pumpkin ls`
reaches the controller from a member's machine.

`sshd` logs `AuthorizedKeysCommand path is not absolute` for each key that the list lacks, as it
reads `none` as the command's path. It refuses the key, as it should.

## What the outputs feed

Each is the module's output of that name. `terraform output` reads it where the root gives it as
an `output` of its own.

- `terraform output -raw client_config` is the `[controller]` table of each member's
  `~/.config/pumpkin/pumpkin.toml`, beside its `[upstreams]` and, for runs on Modal,
  `[controller.modal]`, the Modal provider's profile and classes, and nothing else; `pumpkin help
  config` says what each does, with an example, and which programs a member's machine, Linux on
  x86-64, needs.
- `terraform output -raw client_policy_arn` is the managed IAM policy that a member's machine
  needs. The team's root attaches it to the machine's role or user, or an administrator attaches it
  by hand, as `aws iam attach-role-policy --role-name <role> --policy-arn <arn>` (or
  `attach-user-policy`). `policies/client.json.tftpl` says what it allows. The machine reaches
  port 22 at the controller's private address from a network in `ssh_from`.
- `[gateway] private`, the address the sandboxes dial, is `"<private ip>:0"`, as each job sets it
  on the controller: the groups admit only the ephemeral ports, 32768-60999, where the kernel's
  pick for port 0 falls.

## Policies

The permission policies of the controller's, the timer's, the sandboxes' and the Modal pull roles,
and the client's, are `policies/<principal>.json.tftpl`. This release's `pumpkin-admin policy
<principal>` prints each from what its code's calls on AWS need. The module fills them with
`templatefile`, as the GitHub runner module fills its policies
([policies-runner.tf](https://github.com/github-aws-runners/terraform-aws-github-runner/blob/f975153d6749b2ea2ddebc8d8cf5911196a1973a/modules/runners/policies-runner.tf#L31-L36)).
The files are generated and committed, as Kubebuilder's projects commit the RBAC that
`controller-gen` writes from the code
([makefile.go](https://github.com/kubernetes-sigs/kubebuilder/blob/4d01fcdf146e4d832fc973ef95684a00bcf0f0c0/pkg/plugins/golang/v4/scaffolds/internal/templates/makefile.go#L121-L123)).
`pumpkin`'s tests fail while a file differs from the code's, and `cargo xtask refill --package
pumpkin policy` writes them again. So a change to a call is a change to the code that makes it,
never to these files. The roles' trust policies say who may assume each role. No call states them,
so they are the module's own. So are the AWS-managed policies of the AMI's build, under which no
Pumpkin call is made.

## Changes

An apply that changes a class or the AMI makes a new version of its templates, which the
controller's settings name from its next job on. The controller's policy allows only the
templates' current instance types. So an apply that changes a class's type is a cutover that a
running job would meet: follow the drain runbook under "Versions". A Modal class or profile needs
no apply: it is the client's `[controller.modal]`, which each job takes.

With `variables.tf`'s defaults, the controller costs about $0.94 an hour while it runs in
us-west-1, less in us-east-1 and us-west-2; stopped, about $110 a month for its volume at
us-east-1's rates, more in other regions; beside ECR's storage. A new Ubuntu release or a new
cloud-init reaches it only by `terraform apply -replace=module.pumpkin.aws_instance.controller`,
once no run is live: its volume, and the records on it, go with it.

An apply never changes what Pumpkin installs on the controller: its user, its units and `sshd`'s
settings. A release changes them, when the admin runs the release's install there. As `ubuntu`,
with administrator access, download the archive for the release's `pumpkinversion` line, `<tag>
<commit> <sha256>`. Run its install with the settings' ARN, `terraform output -raw
settings_parameter_arn`. The install keeps the ARN in `/etc/pumpkin/controller.toml`. It reads the
settings' `[deployment]`, so apply this module's settings first. Each job finds the settings by
that file alone, so the install comes before a member's `.pumpkinversion` names the release. The
install says each change it makes, and nothing if it makes none:

```sh
curl --fail --location --output pumpkin.tar.xz \
    https://github.com/FrontierCS/Pumpkin/releases/download/<tag>/pumpkin-<tag>-x86_64-unknown-linux-musl.tar.xz
echo '<sha256>  pumpkin.tar.xz' | sha256sum --check --strict
mkdir pumpkin && tar --extract --xz --file=pumpkin.tar.xz --directory=pumpkin
sudo pumpkin/bin/pumpkin-admin install --settings <settings_parameter_arn>
```

Set `pumpkin_release` to that line too. A controller made later installs that release at its first
boot. A first boot that fails powers the controller off 30 minutes later. `/var/log/cloud-init-output.log`
there says why. Once the cause is fixed,
`terraform apply -replace=module.pumpkin.aws_instance.controller` makes the controller again. At
boot, the controller runs the boot of each installed build. Sandboxes that remain unreachable
expire at the run's hard end.

## Versions

The controller's settings are of a version of the envelope, `M.m`, which their `envelope` key
names. This module writes 1.1. Each job reads the key as it starts, and the host as it copies the
members' keys. Both refuse settings without it.

- A minor adds only what a release of an older minor may safely ignore: a key of the settings, a
  package of the AMI, an IAM action, software on the controller's host. A key that every release
  must honour needs a major. A job refuses a key of the settings that its release doesn't know, as a
  typo. If `envelope` names a later minor than its release knows, the job ignores the key instead,
  and names it in its log. So a minor that adds a key writes `envelope` with it.
- A major removes or changes what a release relies on. A job refuses settings of a major other
  than its release's, and says what to do.
- Every older release refuses a new provider kind, such as `[providers.<name>.gce]`. It also refuses
  a new key beside a provider's kind in `[providers.<name>]`. So either is a major. A job keeps only
  `images` of the Modal provider's table in the settings, as the client sends the rest. It drops any
  other key there without a word in the log.
- A release keeps nothing but the envelope across releases. It need not work with another tag's
  module, controller install or members' clients. So at each tag, the admin applies the tag's
  module. The admin then runs the release's `pumpkin-admin install --settings` on the controller,
  in a cutover as below, as "Changes" says. Each member then raises `.pumpkinversion` to that
  release.
- Each job's directory on the controller is `~pumpkin/jobs/<id>/`. Its `release` names the release
  that runs it, as `pumpkin --version` does. Its `envelope` names the envelopes it takes, such as
  `^1.1`, Cargo's caret requirement: 1.1 or a later 1.x. Step 2 of a cutover, below, lists the ids
  of the jobs that run.

A cutover that a running job would meet drains the controller first. Such a cutover is a major, a
change to the host's software or a replaced controller. Nomad likewise drains a node and lets its
batch jobs complete
([node drain](https://github.com/hashicorp/nomad/blob/a3b86c697f38ab032e1acaae8503ed10815bc4a2/website/content/commands/node/drain.mdx#L9-L22)).
The steps:

1. Set `draining = true` and apply. Each job that starts is refused, and its refusal names the
   drain. The jobs that run go on.
2. Wait until no job runs. On the controller,
   `sudo ls /run/user/$(id --user pumpkin)/pumpkin` lists the id of each job whose unit runs.
   Each job's unit removes the job's directory there as it ends. A run's job ends by the run's
   hard end at the latest. To see which releases still run, `sudo cat ~pumpkin/jobs/<id>/release`
   names the release of job `<id>`.
3. Each member runs `pumpkin wait`, which copies their runs' records.
4. Apply the change, or replace the controller. For a new release, also run its
   `sudo pumpkin/bin/pumpkin-admin install --settings <settings_parameter_arn>` on the controller,
   from its archive, as "Changes" says.
5. Set `draining = false` and apply.
6. At a tag, each member raises `.pumpkinversion` to its release.

## Teardown

```sh
terraform destroy
```

This deletes the repositories with their images, so a record's images can no longer be pulled.
The AMIs that Image Builder baked stay registered, as do their snapshots: deregister each
`aws ec2 describe-images --owners self --filters 'Name=name,Values=pumpkin-<name>*'` lists. The
state bucket and the OIDC provider are the account's, and stay.
