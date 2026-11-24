# deploy/aws

The Terraform module that makes one team's Pumpkin on AWS. The team's admin applies it with
administrator credentials, and lists the members by key. Every name it makes begins
`pumpkin-<name>`, where `<name>` is the team's name, so teams share an account without colliding.
Its state is in an S3 bucket of the account, so no laptop can lose it.

## Once per account

The state bucket, versioned so a bad write can be undone. A team that lists `modal_workspaces`
also needs Modal's OIDC provider, if `aws iam list-open-id-connect-providers` lists no
`oidc.modal.com`:

```sh
aws s3api create-bucket --bucket pumpkin-tfstate-<account>-<region> --region <region> \
    --create-bucket-configuration LocationConstraint=<region>
aws s3api put-bucket-versioning --bucket pumpkin-tfstate-<account>-<region> \
    --versioning-configuration Status=Enabled
aws iam create-open-id-connect-provider --url https://oidc.modal.com --client-id-list oidc.modal.com
```

## Per team

Terraform 1.15.9 or later, and the AWS CLI with administrator credentials for the account. The
module is this directory. A root of the admin's own calls it, pinned by a Pumpkin release's tag, so
a release's `pumpkin-admin install` and its module agree. The root holds what is the deployment's:
the provider's configuration with its region and tags, the backend, and the lock file that
`terraform init` writes. The provider's region is the module's. In a directory of its own, such as
a repository, copy [`examples/team/main.tf`](examples/team/main.tf) and set its `source` as its
comment says (`variables.tf` says each of the module's values and its default). Give its
`pumpkin_release`, the release's `pumpkinversion` line, in a `terraform.tfvars` beside it, or write
the line in place of the variable: `terraform plan` refuses to run without it. Give an `output`, as
the example does, for each other output that "What the outputs feed" or "Versions" names and the
admin reads. `pumpkin` reaches the controller at its private address. Machines in the
controller's VPC reach its port 22, and `sshd` admits for `pumpkin` only the keys that `members`
lists. A machine elsewhere needs a route into the VPC that gives its traffic one of the VPC's
addresses, such as a VPN that translates its addresses into the VPC's. A team of one lists its own
key. A team that enrols no one yet gives `members = {}`, and the controller admits no key.

Then, in that directory:

```sh
terraform init -backend-config=bucket=pumpkin-tfstate-<account>-<region> \
    -backend-config=key=<name>/terraform.tfstate -backend-config=region=<region>
terraform plan -out=pumpkin.tfplan
terraform apply pumpkin.tfplan
```

The first apply waits while Image Builder bakes the AMI on a build instance of its own. It makes
the controller after the bake, as the controller's settings name the templates that boot the AMI. A
concurrent `plan` or `apply` of the same state waits on its lock. To take a later release, set a
new `ref` and its `pumpkin_release`, run `terraform init -upgrade`, and plan, in the cutover under
"Versions". "Versions" also says what a release may change.

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

`grep '^resource' *.tf` here lists them. Besides the moves, the plan creates
`terraform_data.pumpkin_release`, updates the controller's `run` policy, the sandbox's and the
client's to this release's, and destroys the controller's `settings` and `members` policies, which
`run` now holds. The update and the destroys are unordered, so apply it as a cutover, with the
drain runbook under "Versions". Once it is applied, the blocks can go.

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
  port 22 at the controller's private address from the controller's VPC.
- `terraform output -raw contributor_policy_arn` is the managed IAM policy that a contributor's
  machine needs for `cargo xtask live`, which the root attaches to it as it does any policy.
  `policies/contributor.json.tftpl` says what it allows: the client's, what `pumpkin-admin
  check` reads, and the list of the timer's schedules that a run left.
- `[gateway] private`, the address the sandboxes dial, is `"<private ip>:0"`, as each job sets it
  on the controller: the groups admit only the ephemeral ports that `gateway_ports.json` holds,
  where the kernel's pick for port 0 falls.

## Changes

An apply that changes a class or the AMI makes a new version of its templates, which the
controller's settings name from its next job on. The controller's policy allows only the
templates' current instance types. So an apply that changes a class's type is a cutover that a
running job would meet: follow the drain runbook under "Versions". A Modal class or profile needs
no apply: it is the client's `[controller.modal]`, which each job takes.

With `variables.tf`'s defaults, the controller costs about $0.94 an hour while it runs in
us-west-1, less in us-east-1 and us-west-2; stopped, about $110 a month for its volume at
us-east-1's rates, more in other regions; beside ECR's storage.

The controller's first boot runs the install of the release that `pumpkin_release` names. The
install sets up Pumpkin's user, its units and `sshd`'s settings, and nothing changes them later.
So an apply that changes `pumpkin_release` replaces the controller. The new controller also takes
the current Ubuntu release and this module's `controller.yaml`, and its volume is new.
`terraform apply -replace=module.pumpkin.aws_instance.controller` replaces it at the same
release. Either replacement is a cutover, under "Versions".

A first boot that fails powers the controller off 30 minutes later.
`/var/log/cloud-init-output.log` there says why. Once the cause is fixed, an apply that corrects
`pumpkin_release`, or `terraform apply -replace=module.pumpkin.aws_instance.controller`, makes the
controller again. At boot, the controller runs the boot of each installed build. Sandboxes that
remain unreachable expire at the run's hard end.

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
  module with the tag's `pumpkin_release`, in a cutover as below. Each member then raises
  `.pumpkinversion` to that release.
- Each job's directory on the controller is `~pumpkin/jobs/<id>/`. Its `release` names the release
  that runs it, as `pumpkin --version` does. Its `envelope` names the envelopes it takes, such as
  `^1.1`, Cargo's caret requirement: 1.1 or a later 1.x. Step 2 of a cutover, below, lists the ids
  of the jobs that run.

A cutover that a running job would meet drains the controller first. Such a cutover is a major, a
change to the host's software or a replaced controller. Nomad likewise drains a node and lets its
batch jobs complete
([node drain](https://github.com/hashicorp/nomad/blob/a3b86c697f38ab032e1acaae8503ed10815bc4a2/website/content/commands/node/drain.mdx#L9-L22)).
Where CI applies the team's root, each apply below is one merged pull request. A `-replace` is an
apply of its own, outside them.

1. Set `draining = true` and apply. Each job that starts is refused, and its refusal names the
   drain. The jobs that run go on.
2. Wait until no job runs. On the controller,
   `sudo ls /run/user/$(id --user pumpkin)/pumpkin` lists the id of each job whose unit runs.
   Each job's unit removes the job's directory there as it ends. A run's job ends by the run's
   hard end at the latest. `sudo cat ~pumpkin/jobs/<id>/release` names the release of job `<id>`.
   Before a replacement, each member runs `pumpkin wait <id>` for each run whose record they
   keep. A replacement discards each run's record that no member copied.
3. Apply the change. For a release, raise the module's `ref` and `pumpkin_release` to the
   release's. Its plan replaces `module.pumpkin.aws_instance.controller`, and the new
   controller's first boot installs the release. The apply that creates
   `terraform_data.pumpkin_release` replaces nothing, as only a change of it does. So where the
   plan creates it, apply with `-replace=module.pumpkin.aws_instance.controller`. The new
   controller has a new host key, so each member's first command after the replacement is
   refused. Once the admin confirms the replacement, the member runs the `ssh-keygen -R` command
   that the refusal names, and the next command pins the new key.
4. Once `pumpkin ls` reaches the controller, set `draining = false` and apply.
5. At a tag, each member raises `.pumpkinversion` to its release.

## Teardown

```sh
terraform destroy
```

This deletes the repositories with their images, so a record's images can no longer be pulled.
The AMIs that Image Builder baked stay registered, as do their snapshots: deregister each
`aws ec2 describe-images --owners self --filters 'Name=name,Values=pumpkin-<name>*'` lists. The
state bucket and the OIDC provider are the account's, and stay.
