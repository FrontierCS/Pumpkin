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
| the timer: a schedule group and a role that terminates run instances | a kill bound outside each guest |
| the Modal pull role: ECR pulls from `roles`, assumed only by `modal_workspace`'s tokens | Modal imports the images of calls' targets, which hold hidden data |
| an AMI baked by EC2 Image Builder from a named AL2023 release, with Docker, ECR's credential helper and nftables | each sandbox's boot runs them, at the release's versions |
| the controller's role and instance profile: access to its jobs' settings parameter and the EC2 provider's resources | the runs' launches, timers, sweeps and pushes, from the controller |
| the controller: an on-demand instance on Ubuntu 24.04 LTS, with a public IPv4 address, IMDSv2 within one hop, an encrypted gp3 root, stopping on shutdown, tagged `pumpkin:controller=<name>`; an apply never replaces it | builds the tasks' images and runs the drivers, which a person's machine sends it, and runs no role |
| what cloud-init installs there: Docker, which runs as root, so its `docker` group, which holds `pumpkin` alone, is root-equivalent; Buildx, AWS CLI v2 and uv at exact versions, and the rest of `controller.yaml`'s packages from a snapshot of Ubuntu's archive; the user `pumpkin`, lingering; an `sshd` that admits concurrent openings and ends a session whose client has gone; a unit that stops the controller after 30 minutes with no job and no SSH session, and one that runs each registered build's reap at boot | builds with the containerd image store; jobs that outlive every connection; a stop and a reap that need no client |
| the SSM parameter `/pumpkin-<name>/settings` (advanced tier): the envelope, the Docker engine, the EC2 provider `aws`, and, with `modal_classes`, the Modal provider `modal` | each job's settings, which an apply changes for the next job |
| the controller role's read of `/pumpkin-<name>/modal-token`, a SecureString that the admin writes, not the module | the team's Modal token, which each job's Modal provider reads, and no state, file or client holds |
| with `public_page`: a private bucket `pumpkin-<name>-public-…` that a CloudFront distribution alone reads, and the controller's right to list it and upload to it | the public page, below |

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

For runs on Modal, the team's runs share one workspace, `modal_workspace`, which the controller
reaches with a service user's token, as GitHub's and Buildkite's runners get their credentials
from the control plane, not from each member. With the workspace's owner or a manager, and the
CLI at the helper's version, `uvx --from modal==1.6.3 modal`:

1. Make a service user, with its token in a file that only you can read, so that no terminal or
   shell history shows it, and give it Contributor in the environment `main`, as Modal's
   [service users](https://modal.com/docs/guide/service-users) guide says:

   ```sh
   (umask 077 && modal workspace service-users create pumpkin-<name>-1 --json > modal-token.json)
   modal workspace service-users set-role pumpkin-<name>-1 main contributor
   ```

2. Write the token into the parameter that the controller reads, under SSM's own key, `aws/ssm`,
   which the controller's role decrypts through SSM with no grant of its own (so name no
   `--key-id`), then remove the file:

   ```sh
   aws ssm put-parameter --region <region> --name /pumpkin-<name>/modal-token \
       --type SecureString --overwrite --value file://modal-token.json
   rm modal-token.json
   ```

3. Name the classes in `modal_classes`, in `terraform.tfvars`, and apply; `variables.tf` says each
   key:

   ```hcl
   modal_classes = {
     a100 = { gpu = "A100-80GB", gpu_model = "NVIDIA A100-SXM4-80GB", cpu_millis = 8000, memory_bytes = 68719476736, startup_seconds = 1800 }
   }
   ```

With a class in `modal_classes`, every run on the controller reaches the workspace as it starts,
an EC2 run's too, so a run fails while the parameter holds no token that Modal takes.

Modal makes no new token for a service user, so a rotation is a new service user. Every member's
build runs as the controller's role, which reads the token, so rotate it when a member leaves:

1. Make `pumpkin-<name>-2` and write its token, as in steps 1 and 2. Each run that starts from then
   on reaches the workspace with it.
2. Delete `pumpkin-<name>-1` with `modal workspace service-users delete` once the runs that
   started before it have ended, by draining as below: a run reads the token once, so one that
   still runs with the old token can no longer delete its sandboxes, which then end at its hard
   end, and `pumpkin reap` removes what is left.

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
  `~/.config/pumpkin/pumpkin.toml`, beside its `[upstreams]`, and nothing else; `pumpkin help
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
  the controller's settings hold already, and `modal_token_parameter` the parameter that holds
  the token.
- `[gateway] private`, the address the sandboxes dial, is `"<private ip>:0"`, as each job sets it
  on the controller: the groups admit only the ephemeral ports, 32768-60999, where the kernel's
  pick for port 0 falls.

## Changes

An apply that changes a class or the AMI makes a new version of its templates, which the
controller's settings name from its next job on, as the `ec2` output does, and so does a change
of `modal_classes`. A new Modal token needs no apply.

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

## Versions

The controller's settings are of a version of the envelope, `M.m`, which each job reads from
their `envelope` key as it starts; this module writes 2.0, and settings without one are of 1.0.

- A minor adds only what a release of an older minor may safely ignore: a key of the settings, a
  package of the AMI, an IAM action, software on the controller's host. A key that every release
  must honour needs a major. A job refuses a key of the settings that its release doesn't know, as
  a typo, unless `envelope` names a later minor than its release knows; then it ignores the key,
  and names it in its log. So a minor that adds a key writes `envelope` with it.
- A major removes or changes what a release relies on. A job refuses settings of a major other
  than its release's, and says what to do.
- A new provider kind, such as `[providers.<name>.gce]`, or a new key beside a provider's kind in
  `[providers.<name>]`, is refused by every older release, so either is a major.
- An upgrade changes the envelope first, then the releases, and removes what an older release
  relied on only once no pinned release does, as Kubernetes upgrades its API server before its
  kubelets
  ([version skew policy](https://github.com/kubernetes/website/blob/b2fed83accab4d0a607783d21740e7de43551369/content/en/releases/version-skew-policy.md#L148-L186)).
- The module serves the releases of the current tag and of the one before it, as Consul stays
  compatible with at least one prior version
  ([compatibility](https://github.com/hashicorp/web-unified-docs/blob/4ec2d604280fff09cfca1d2648fed5cb51f81f14/content/consul/v1.22.x/content/docs/upgrade/compatibility.mdx#L22-L33)).
- A release that reads no `envelope` refuses settings that hold a key it doesn't know, `envelope`
  among them.
- In envelope 2.x, the Modal provider is the settings', with its token and classes, and a
  client's `[controller.modal]` is refused. A release of 1.x refuses 2.x's settings, and one of
  2.x refuses 1.x's.
- Each job's directory on the controller, `~pumpkin/jobs/<id>/` or `~pumpkin/builds/<id>/`, names
  the release that runs it in `release`, as `pumpkin --version` does, and the envelopes it takes
  in `envelope`, such as `^1.0`, Cargo's caret requirement: 1.0 or a later 1.x. Step 2 of a cutover,
  below, lists the ids of the jobs that run. A release from before these files writes neither.

A cutover that a running job would meet, such as a major, a host's software or a replaced
controller, drains the controller first, as Nomad drains a node, letting its batch jobs complete
([node drain](https://github.com/hashicorp/nomad/blob/a3b86c697f38ab032e1acaae8503ed10815bc4a2/website/content/commands/node/drain.mdx#L9-L22)):

1. Set `draining = true` and apply. Each job that starts is refused, naming the drain; the jobs
   that run go on. A release that doesn't know `draining` refuses the settings too, as it
   refuses any key of envelope 1.0 that it doesn't know.
2. Wait until no job runs, a build's included: on the controller,
   `sudo ls /run/user/$(id --user pumpkin)/pumpkin` lists the id of each job whose unit runs,
   as its unit removes the job's directory there as it ends. A run's job ends by the run's hard
   end at the latest; `pumpkin ls` lists the live runs, but not the builds.
3. Each member runs `pumpkin wait`, which copies their runs' records.
4. Apply the change, or replace the controller.
5. Set `draining = false` and apply.
6. Each member raises `.pumpkinversion` if the envelope's major changed.

Step 1 applies from the module's revision that the controller has, so that the drain's message
reaches releases that know it. A run that a release of an older major made is reaped only by a
client of that release; one that it lost ends by its hard end.

From 1.x to 2.x, at step 4 the admin writes the Modal token and names `modal_classes`, as above,
from the classes of the admin's own `[controller.modal]`; at step 6 each member removes
`[controller.modal]` from `pumpkin.toml`.

## The public page

A read-only page of the controller's runs, for a team that knows its address: CloudFront serves it
over HTTPS from a private bucket, at the distribution's own `https://<id>.cloudfront.net/`, with
no login and kept out of search engines. The controller's jobs upload the runs, live and ended.

Three variables in `terraform.tfvars`, which `variables.tf` says, set it up: `public_page`,
`publish_from_jobs` and `publish_since`, such as `"2026-11-15T00:00:00Z"`.

- The controller role's policy `publish`, which `public_page` gives it, has no read and no
  delete. `terraform output -raw public_page` is the address, and `public_bucket` the bucket.
- Set `publish_from_jobs` once the controller runs a Pumpkin that knows the `[publish]` table: an
  older one refuses the settings that hold it.

Then subscribe the distribution to CloudFront's free flat-rate plan, in the console: past the
always-free 1 TB and 10 million requests a month, pay-as-you-go charges about $85 a TB, which a
downloader set on it could run up, and the free plan charges none, slowing the page instead (its
[terms](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/flat-rate-pricing-plan.html)).

The page costs about $2 a month to keep, estimated for 150 runs a month of 2 hours each, at
us-east-1's rates: S3's uploads, about 360,000 a month, $1.80, and its lists, $0.40; the files,
about 0.3 GB, a cent; CloudFront, nothing within its free tier. The first publish uploads the
controller's older runs, about $0.60 once.

To take a run down by hand, with administrator credentials: take it out of the list, which the
bucket holds deflated in zlib's format, as `Content-Encoding: deflate`, remove its prefix, and
invalidate it, so that no edge serves it from its cache. The next publish uploads it again while
the controller holds its record.

```sh
aws s3 cp s3://<bucket>/runs.json runs.json.z
python3 -c 'import sys, zlib; sys.stdout.buffer.write(zlib.decompress(open(sys.argv[1], "rb").read()))' runs.json.z > runs.json
python3 -c 'import json, sys; d = json.load(open(sys.argv[1], "rb")); d["runs"] = [r for r in d["runs"] if r["id"] != sys.argv[2]]; open(sys.argv[1], "w", encoding="utf-8").write(json.dumps(d, ensure_ascii=False, separators=(",", ":")))' runs.json <id>
python3 -c 'import sys, zlib; sys.stdout.buffer.write(zlib.compress(open(sys.argv[1], "rb").read()))' runs.json > runs.json.z
aws s3 cp runs.json.z s3://<bucket>/runs.json --content-encoding deflate \
    --content-type application/json --cache-control 'public, max-age=30'
aws s3 rm s3://<bucket>/runs/<id>/ --recursive
aws cloudfront list-distributions --query "DistributionList.Items[?DomainName=='<host of public_page>'].Id" --output text
aws cloudfront create-invalidation --distribution-id <distribution> --paths '/runs/<id>/*' /runs.json
```

To take the page down, set `public_page` and `publish_from_jobs` to `false`, remove
`publish_since`, and apply: the apply empties and deletes the bucket, whose files are copies of
the controller's records.

## Teardown

```sh
terraform destroy
```

This deletes the repositories with their images, so a record's images can no longer be pulled.
The AMIs that Image Builder baked stay registered, as do their snapshots: deregister each
`aws ec2 describe-images --owners self --filters 'Name=name,Values=pumpkin-<name>*'` lists. The
state bucket and the OIDC provider are the account's, and stay.
