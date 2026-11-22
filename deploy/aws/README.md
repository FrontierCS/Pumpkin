# deploy/aws

The Terraform module that makes one person's Pumpkin on AWS, which that person applies with their
own administrator credentials, from their own machine. Every name it makes begins
`pumpkin-<name>`, so teammates share an account without colliding, and its state is in an S3
bucket of the account, so no laptop can lose it.

| What | Why |
| --- | --- |
| the sandboxes' security group: out to HTTPS and to the controller's ephemeral ports; nothing in | sandboxes pull from ECR and dial the gateway at port 0's pick, and reach nothing else |
| the controller's security group: in from the sandboxes on the ephemeral ports, and from `ssh_from`, none by default, on port 22; out to HTTPS | the gateway, and `sshd` for the person's machine alone |
| ECR `pumpkin-<name>/roles` and `/sandboxd`, immutable tags, untagged images expire after 14 days | private images, pushed tagged by digest |
| a launch template per class: the AMI, the type, IMDSv2 within one hop, a root volume deleted with the instance | the EC2 classes, and what the `ec2` provider relies on |
| the sandboxes' role: ECR pulls only | each sandbox's pulls |
| the timer: a schedule group and a role that terminates the run instances of this envelope's sandbox profile | a kill bound outside each guest |
| the Modal pull role: ECR pulls from `roles`, assumed only by `modal_workspace`'s tokens | Modal imports the images of calls' targets, which hold hidden data |
| an AMI baked by EC2 Image Builder from a named AL2023 release, with Docker, ECR's credential helper and nftables | each sandbox's boot runs them, at the release's versions |
| the controller's role and instance profile: access to its jobs' settings parameter and the EC2 provider's resources: launches from its templates with nothing they name overridden, and terminations and console reads of its own sandboxes alone | the runs' launches, timers, sweeps and pushes, from the controller |
| the controller: an on-demand instance on Ubuntu 24.04 LTS, with a public IPv4 address, IMDSv2 within one hop, an encrypted gp3 root, stopping on shutdown, tagged `pumpkin:controller=<name>`; an apply never replaces it | builds the tasks' images and runs the drivers, which a person's machine sends it, and runs no role |
| what cloud-init installs there: Docker, which runs as root, so its `docker` group, which holds `pumpkin` alone, is root-equivalent; Buildx, AWS CLI v2 and uv at exact versions, and the rest of `controller.yaml`'s packages from a snapshot of Ubuntu's archive; the user `pumpkin`, lingering; an `sshd` that admits concurrent openings and ends a session whose client has gone; a unit that stops the controller after 30 minutes with no job and no SSH session, and one that runs each registered build's reap at boot | builds with the containerd image store; jobs that outlive every connection; a stop and a reap that need no client |
| the SSM parameter `/pumpkin-<name>/settings` (advanced tier): the Docker engine, the EC2 provider `aws`, and the Modal provider `modal`'s `images` | each job's settings, which an apply changes for the next job; the rest of the Modal provider is the client's |

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

## Per person

Terraform 1.11 or later, and the AWS CLI with administrator credentials for the account. In this
directory, write `terraform.tfvars` (`variables.tf` says each value and its default):

```hcl
name            = "alice"
region          = "us-west-1"
vpc_id          = "vpc-…"
subnet_id       = "subnet-…"
modal_workspace = "ac-…"
client_roles    = ["alice-laptop"] # the IAM roles and users of the person's machine
client_users    = []
ssh_from        = ["10.0.1.23/32"] # the person's machine's private address
```

`pumpkin` reaches the controller at its private address, so the person's machine is in the
controller's VPC, or routed to it, and `ssh_from` admits it.

Then:

```sh
terraform init -backend-config=bucket=pumpkin-tfstate-<account>-<region> \
    -backend-config=key=<name>/terraform.tfstate -backend-config=region=<region>
terraform plan -out=pumpkin.tfplan
terraform apply pumpkin.tfplan
```

The first apply waits while Image Builder bakes the AMI on a build instance of its own. A
concurrent `plan` or `apply` of the same state waits on its lock.

## What the outputs feed

- `terraform output -raw client_config` is the `[controller]` table of the person's
  `~/.config/pumpkin/pumpkin.toml`, beside its `[upstreams]` and, for runs on Modal,
  `[controller.modal]`, the Modal provider's profile and classes, and nothing else; `pumpkin help
  config` says what each does, with an example, and which programs the person's machine, Linux on
  x86-64, needs.
- `terraform output -raw client_policy_arn` is the managed IAM policy that the person's machine
  needs; an account whose roles live elsewhere attaches it by hand, as `aws iam attach-role-policy
  --role-name <role> --policy-arn <arn>` (or `attach-user-policy`). `outputs.tf` says what it
  allows. The machine reaches port 22 at the controller's private address from a network in
  `ssh_from`.
- `terraform output -raw ec2` is the body of the EC2 provider's table in `pumpkin.toml`, under a
  header such as `[providers.aws.ec2]`, whose keys `pumpkin help config` says, with each class's
  template at the version its creates launch. The controller's settings hold it already; a machine
  that runs Pumpkin itself in the controller's security group takes it from here.
- `terraform output -raw modal_images` is the `images` key of the Modal provider's table, which
  the controller's settings hold already.
- `[gateway] private`, the address the sandboxes dial, is `"<private ip>:0"`, as each job sets it
  on the controller: the groups admit only the ephemeral ports, 32768-60999, where the kernel's
  pick for port 0 falls.

## Changes

An apply that changes a class or the AMI makes a new version of its templates, which the
controller's settings name from its next job on, as the `ec2` output does. A Modal class or
profile needs no apply: it is the client's `[controller.modal]`, which each job takes.

With `variables.tf`'s defaults, the controller costs about $0.94 an hour while it runs in
us-west-1, less in us-east-1 and us-west-2; stopped, about $110 a month for its volume at
us-east-1's rates, more in other regions; beside ECR's storage. A new Ubuntu release or a new
cloud-init reaches it only by `terraform apply -replace=aws_instance.controller`, once no run is
live: its volume, and the records on it, go with it.

To upgrade boot recovery on an existing instance while keeping its volume, let older jobs end
and run `pumpkin reap` from their matching client builds. With administrator access, install
this module's `pumpkin-boot` at `/usr/local/bin/pumpkin-boot` (0755), install the
`pumpkin-boot.service` from `controller.yaml` at `/etc/systemd/system/pumpkin-boot.service`,
and reload the units with `systemctl daemon-reload`. The next boot runs each job install
that published a `bin/pumpkin-boot` link. Older installs need their matching client to reap
subsequent jobs. Sandboxes that remain unreachable expire at the run's hard end.

## Teardown

```sh
terraform destroy
```

This deletes the repositories with their images, so a record's images can no longer be pulled.
The AMIs that Image Builder baked stay registered, as do their snapshots: deregister each
`aws ec2 describe-images --owners self --filters 'Name=name,Values=pumpkin-<name>*'` lists. The
state bucket and the OIDC provider are the account's, and stay.
