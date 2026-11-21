# The envelope

The envelope is what Pumpkin's code relies on of its deployment on AWS: the settings parameter
that a controller's jobs read, what each IAM principal may do, the tags, the launch templates, the
sandbox AMI, the network and the controller's host. [`deploy/aws`](../deploy/aws/README.md) is one
module that makes an envelope; any other that meets this page serves the same releases. Each item
names the code that relies on it, as `path:line` at the commit that last changed this page, and
each IAM action has the resource types and condition keys that AWS's
[Service Authorization Reference](https://docs.aws.amazon.com/service-authorization/latest/reference/)
gives it.

Pumpkin checks little of this before it acts. `Ec2::connect` checks that AWS answers to the
caller's credentials and describes both repositories (`providers/ec2/src/lib.rs:201-230`), and a
job checks that its settings parse. Anything else that the envelope lacks fails the call that
needs it, or an instance's boot, which its console shows.

## The settings parameter

The settings of a controller's jobs, which the module writes and each job reads as it starts.

- **Name:** `/pumpkin-<name>/settings`, where `<name>` is the client's `[controller] name`
  (`crates/pumpkin/src/controller/send.rs:39-40`), in the client's `[controller] region`
  (`send.rs:211-212`), both passed to the job's unit as `--region` and `--parameter`
  (`crates/pumpkin/src/job/start.rs:139-147`).
- **Type:** `String`. The job runs `aws ssm get-parameter --name <parameter>` without
  `--with-decryption` (`crates/pumpkin/src/job.rs:375-376`), so a `SecureString` would reach it
  encrypted. The value holds no secret: names, IDs and ARNs.
- **Format:** TOML, a machine's configuration but for the tables that the job adds. The job refuses
  settings that hold `[gateway]` or `[upstreams]` (`job.rs:381-387`), sets `[gateway] private` to
  the controller's private IPv4 address at port 0 (`job.rs:388-389`), takes `[upstreams]` from the
  client, replaces each Modal provider's table with the client's but its `images`
  (`job.rs:432-466`), then parses the whole (`config::read`, `crates/pumpkin/src/config.rs:332`;
  `crates/pumpkin/src/run.rs:287`, `job.rs:169`). A `[controller]` table makes the file a client's
  configuration, which a job refuses (`config.rs:405-428`, `:344-352`).
- **Unknown keys:** every table refuses a key it doesn't know (`#[serde(deny_unknown_fields)]`,
  `config.rs:44`, `:101`, `:108`; `providers/ec2/src/lib.rs:64`, `:83`, `:95`, `:110`;
  `providers/modal/src/lib.rs:68`, `:84`; `crates/pumpkin/src/publish.rs:60`). A key that a
  module writes before a release knows it fails each job of every older release at its start.

| Key | Type | Required | Read by |
| --- | --- | --- | --- |
| `[docker] context` | a Docker context's name, not empty (`engine::ContextName`, `crates/engine/src/lib.rs:93-113`) | yes (`config.rs:440`) | the job's builds and pushes, on that engine (`run.rs:282-287`) |
| `[docker] cpus` | a CPU list (`docker::CpuList`) | no | the Docker provider, which a controller's settings don't name |
| `[providers.<p>.ec2] region` | an AWS region (`aws::Region`, `crates/aws/src/lib.rs:53-78`) | yes | every `aws` call of the EC2 provider (`providers/ec2/src/lib.rs:66`, `:208`) |
| `repositories.roles`, `repositories.sandboxd` | ECR repository names in `region` (`aws::RepositoryName`) | yes | `Ec2::connect`, which fails unless AWS describes both (`lib.rs:93-101`, `:353-364`) |
| `timer.group` | a Scheduler schedule group's name | yes | each create's schedule and each delete's (`lib.rs:82-91`, `launch.rs:171-205`, `:375-389`) |
| `timer.role_arn` | an IAM role's ARN (`aws::RoleArn`, `aws/src/lib.rs:90-134`) | yes | each schedule's target (`launch.rs:179-186`) |
| `classes.<class>` | a table, `<class>` a Pumpkin name, `[a-z][a-z0-9_-]{0,63}` (`crates/schema/src/ir/values.rs:160-173`) | no | the class of sandbox it offers (`lib.rs:109-124`, `:318-342`) |
| `classes.<class>.template` | a launch template's ID | yes | each create (`launch.rs:132-143`) |
| `classes.<class>.version` | a positive integer: the template's version that each create launches | yes | each create (`launch.rs:132-135`) |
| `classes.<class>.vcpus`, `.memory_mib` | positive integers: the instance type's vCPUs and its memory in MiB | yes | what the class offers a role (`lib.rs:126-167`) |
| `classes.<class>.cpu_features` | CPU flags as `/proc/cpuinfo` names them (`values.rs:526-536`) | no: none | what the class offers a role |
| `classes.<class>.cpu_model` | a CPU model as `/proc/cpuinfo` names it (`values.rs:538-547`) | no: none | what the class offers a role |
| `classes.<class>.usd_per_hour` | a price from 0 to 1000000 (`crates/schema/src/workflow.rs:223-244`) | no: none | the run's record of its spend |
| `classes.<class>.startup_seconds` | a positive integer | no: 1740 (`lib.rs:43-48`) | the bound of a sandbox's start |
| `[providers.<p>.modal] images.region`, `.repository`, `.role_arn` | a region; an ECR repository there; the Modal pull role's ARN (`providers/modal/src/lib.rs:81-90`) | for a client that sends `[controller.modal]` (`job.rs:450-458`) | Modal's import of each image (`modal/src/lib.rs:273-283`, `:363-371`) |
| `[publish] bucket` | an S3 bucket's name (`publish.rs:72-98`) | in `[publish]` | each publish's uploads (`publish.rs:254-293`) |
| `[publish] distribution` | a string | in `[publish]` | no code: it names the distribution for a take-down (`publish.rs:62-67`) |
| `[publish] address` | a string | in `[publish]` | the job's log (`publish.rs:226-228`) |
| `[publish] since` | an offset date-time (`publish.rs:100-150`) | no | which runs a publish leaves out |

The EC2 provider's `vcpus` and `memory_mib` must be its template's instance type's, and `version`
must name a version of `template`; nothing checks either on AWS.

## IAM, by principal

Each `aws` call that Pumpkin makes runs through `aws::Cli`, in the settings' or the client's region,
with the credentials that `aws` finds itself (`crates/aws/src/lib.rs:1-7`, `:392-410`). The reference
pages: [EC2](https://docs.aws.amazon.com/service-authorization/latest/reference/list_amazonec2.html),
[EC2 Instance Connect](https://docs.aws.amazon.com/service-authorization/latest/reference/list_amazonec2instanceconnect.html),
[ECR](https://docs.aws.amazon.com/service-authorization/latest/reference/list_amazonelasticcontainerregistry.html),
[EventBridge Scheduler](https://docs.aws.amazon.com/service-authorization/latest/reference/list_amazoneventbridgescheduler.html),
[IAM](https://docs.aws.amazon.com/service-authorization/latest/reference/list_awsidentityandaccessmanagementiam.html),
[S3](https://docs.aws.amazon.com/service-authorization/latest/reference/list_amazons3.html),
[STS](https://docs.aws.amazon.com/service-authorization/latest/reference/list_awssecuritytokenservice.html)
and [Systems Manager](https://docs.aws.amazon.com/service-authorization/latest/reference/list_awssystemsmanager.html).
`sts:GetCallerIdentity` needs no permission, and an explicit deny doesn't stop it
([`GetCallerIdentity`](https://docs.aws.amazon.com/STS/latest/APIReference/API_GetCallerIdentity.html)).

### The controller's role

The instance profile of the controller, whose jobs run the EC2 and Modal providers and publish.
A machine that runs the EC2 provider itself, with no controller, needs the same, but
`ssm:GetParameter`.

| Action | Call | Resource it needs | Resource type, condition keys |
| --- | --- | --- | --- |
| `sts:GetCallerIdentity` | `aws sts get-caller-identity` (`providers/ec2/src/lib.rs:209`, `:239`) | none | none: needs no permission |
| `ecr:DescribeRepositories` | `aws ecr describe-repositories` (`crates/aws/src/lib.rs:270-291`), for both repositories (`providers/ec2/src/lib.rs:359`) and Modal's (`providers/modal/src/lib.rs:274`) | `roles`, `sandboxd` | `repository*`; `aws:ResourceTag/${TagKey}` |
| `ecr:BatchGetImage` | `aws ecr batch-get-image`, whether a repository holds an image (`aws/src/lib.rs:327-343`) | `roles`, `sandboxd` | `repository*` |
| `ecr:GetAuthorizationToken` | `aws ecr get-login-password` (`aws/src/lib.rs:379`) | `*` | no resource type |
| `ecr:BatchCheckLayerAvailability`, `ecr:InitiateLayerUpload`, `ecr:UploadLayerPart`, `ecr:CompleteLayerUpload`, `ecr:PutImage` | the engine's push of an image (`aws/src/lib.rs:367-390`), as [ECR's push policy](https://docs.aws.amazon.com/AmazonECR/latest/userguide/image-push-iam.html) lists them | `roles`, `sandboxd` | `repository*` |
| `ec2:RunInstances` | `aws ec2 run-instances --launch-template … --instance-initiated-shutdown-behavior terminate --tag-specifications … --user-data …` (`providers/ec2/src/launch.rs:132-154`) | the envelope's launch templates, and only the resources each names | each of `image*`, `instance*`, `network-interface*`, `security-group*`, `subnet*`, `volume`, `launch-template`; `ec2:LaunchTemplate`, `ec2:IsLaunchTemplateResource` |
| `ec2:CreateTags` | the same call's tags, `pumpkin:run`, `pumpkin:role` and `Name` on the instance and its volumes (`launch.rs:589-606`) | new instances and volumes | `instance`, `volume`; `ec2:CreateAction` = `RunInstances`, `aws:TagKeys` |
| `iam:PassRole` | the same call, of the template's instance profile's role | the sandbox role | `role*`; `iam:PassedToService` = `ec2.amazonaws.com` |
| `ec2:DescribeInstances` | `aws ec2 describe-instances --filters` by `pumpkin:run` or by ID (`launch.rs:429-435`) | `*` | no resource type |
| `ec2:TerminateInstances` | `aws ec2 terminate-instances` of a run's instance, one found by ID and checked by its `pumpkin:run` (`launch.rs:324-356`, `:437-442`) | the envelope's sandbox instances | `instance*`; `aws:ResourceTag/${TagKey}`, `ec2:InstanceProfile` |
| `ec2:GetConsoleOutput` | `aws ec2 get-console-output --latest` of a run's instance (`launch.rs:237-267`) | the envelope's sandbox instances | `instance*`; `aws:ResourceTag/${TagKey}`, `ec2:InstanceProfile` |
| `scheduler:CreateSchedule` | `aws scheduler create-schedule` of an instance's timer (`launch.rs:164-211`) | schedules in the timer's group | `schedule*` (`schedule/<group>/<name>`) |
| `iam:PassRole` | the same call, of the timer's role | the timer role | `role*`; `iam:PassedToService` = `scheduler.amazonaws.com` |
| `scheduler:DeleteSchedule` | `aws scheduler delete-schedule` once its instance is terminated (`launch.rs:374-389`) | schedules in the timer's group | `schedule*` |
| `ssm:GetParameter` | the job's read of its settings (`crates/pumpkin/src/job.rs:375-376`) | the settings parameter | `parameter*` |
| `s3:ListBucket` | `aws s3 sync` (`crates/pumpkin/src/publish.rs:396-440`), with `[publish]` | the page's bucket | `bucket` |
| `s3:PutObject` | `aws s3 sync` and `aws s3 cp` (`publish.rs:396-440`), with `[publish]` | the bucket's objects | `object` |

The EC2 reference's operations table lists `ec2:RunInstances`, `ec2:CreateTags` and `iam:PassRole`
(with `iam:PassedToService` = `ec2.amazonaws.com`) for `RunInstances`, and Scheduler's lists
`iam:PassRole` (with `iam:PassedToService` = `scheduler.amazonaws.com`) for `CreateSchedule`. A
schedule's deletion after it fires needs no permission of either role
([deleting a schedule](https://docs.aws.amazon.com/scheduler/latest/UserGuide/managing-schedule-delete.html)).
Pumpkin terminates and reads the console of no instance but one its run launched, so the scope
needed is the instances that carry `pumpkin:run` and the envelope's sandbox instance profile.

### The sandbox instances' role

The instance profile of each launch template. Root's Docker on the instance pulls the role's image
and the image of `sandboxd`, asking ECR's credential helper for a token with this role on each pull
(`providers/ec2/src/user_data.rs:67-73`; `providers/ec2/boot/src/main.rs:223-240`; the boot unit's
`docker create`, `providers/ec2/src/pumpkin-ec2-boot.service:36`).

| Action | Resource it needs | Resource type |
| --- | --- | --- |
| `ecr:GetAuthorizationToken` | `*` | no resource type |
| `ecr:BatchGetImage`, `ecr:GetDownloadUrlForLayer` | `roles`, `sandboxd` | `repository*` |

These are the actions of AWS's
[`AmazonEC2ContainerRegistryPullOnly`](https://docs.aws.amazon.com/aws-managed-policy/latest/reference/AmazonEC2ContainerRegistryPullOnly.html)
policy but `ecr:BatchImportUpstreamImage`, which only a pull-through cache needs.

### The timer's role

The role that each schedule's target runs with (`launch.rs:179-186`): EventBridge Scheduler's
universal target `arn:<partition>:scheduler:::aws-sdk:ec2:terminateInstances`, given the
instance's ID.

| Action | Resource it needs | Resource type, condition keys |
| --- | --- | --- |
| `ec2:TerminateInstances` | the envelope's sandbox instances | `instance*`; `aws:ResourceTag/${TagKey}`, `ec2:InstanceProfile` |

Its trust admits `scheduler.amazonaws.com` for the timer's group alone, by `aws:SourceAccount` and
`aws:SourceArn`, as
[Scheduler's confused deputy guide](https://docs.aws.amazon.com/scheduler/latest/UserGuide/cross-service-confused-deputy-prevention.html)
asks.

### The Modal pull role

The role that Modal assumes, by its own OIDC identity, to import a role's image: the helper hands
Modal its ARN and region and no credential (`providers/modal/helper/pumpkin-modal.py:83-93`).

| Action | Resource it needs | Resource type |
| --- | --- | --- |
| `sts:AssumeRoleWithWebIdentity` (its trust) | the role, for tokens of the operator's workspace alone | `role*`; `oidc.modal.com:aud`, `oidc.modal.com:sub` |
| `ecr:GetAuthorizationToken` | `*` | no resource type |
| `ecr:BatchGetImage`, `ecr:GetDownloadUrlForLayer`, `ecr:BatchCheckLayerAvailability` | `images.repository` | `repository*` |

[Modal's guide](https://modal.com/docs/guide/private-registries) lists these among more read
actions for ECR; these three serve an import, which `cargo xtask live modal` exercises. A token's
`sub` begins
`modal:workspace_id:<workspace>:` ([Modal's OIDC guide](https://modal.com/docs/guide/oidc-integration)).

### The client

The machine whose configuration names `[controller]`: it finds the controller, starts it and
pushes its key for the controller's user (`crates/pumpkin/src/controller/aws.rs`).

| Action | Call | Resource it needs | Resource type, condition keys |
| --- | --- | --- | --- |
| `ec2:DescribeInstances` | by the controller's tag, then by ID (`aws.rs:115-140`, `:188-193`) | `*` | no resource type |
| `ec2:StartInstances` | `aws ec2 start-instances` of a stopped controller (`aws.rs:182-186`) | the controller | `instance*` |
| `ec2-instance-connect:SendSSHPublicKey` | `aws ec2-instance-connect send-ssh-public-key --instance-os-user pumpkin` (`aws.rs:297-313`) | the controller | `instance*`; `ec2:osuser` = `pumpkin` |

### `cargo xtask live`

The credentials of the machine that runs it, a client of the controller
(`xtask/src/live.rs:344-363`, `:575-635`).

| Action | Call | Resource it needs |
| --- | --- | --- |
| `sts:GetCallerIdentity` | `aws sts get-caller-identity` (`live.rs:350`) | none |
| `ec2:DescribeInstances` | a run's instances left behind, by `pumpkin:run` (`live.rs:587-588`) | `*` |
| `scheduler:ListSchedules` | a run's schedules left behind, by name prefix (`live.rs:622-635`) | `*`: no resource type |

## Tags and names

- `pumpkin:controller=<name>` on the controller's instance, by which the client finds it: exactly
  one instance that isn't shutting down or terminated (`crates/pumpkin/src/controller/aws.rs:21`,
  `:115-130`). The client pins the host key by the controller's name, not by the instance
  (`crates/pumpkin/src/controller.rs:220-222`).
- `pumpkin:run=<run>`, `pumpkin:role=<role>` and `Name=<run>.<sandbox>` on each sandbox's
  instance and volumes, set as it is launched (`providers/ec2/src/launch.rs:26-29`, `:589-606`). A
  run finds its instances by `pumpkin:run` (`launch.rs:468-470`) and terminates none without it
  (`launch.rs:333-340`); the IAM scope of termination relies on it.
- Each instance's schedule is `<run>.<instance>`, in the timer's group (`launch.rs:445-448`).

## Launch templates

Each class's `template`, at its `version`, which the provider launches with these parameters of
its own (`providers/ec2/src/launch.rs:132-152`): `--instance-initiated-shutdown-behavior
terminate`, the tags, and the user data, which replaces any of the template's. The template gives
the rest, and the provider relies on it (`providers/ec2/src/lib.rs:14-21`):

- an AMI that meets [the sandbox AMI](#the-sandbox-ami);
- an instance type built on the Nitro System, as only those serve the latest serial console that
  `get-console-output --latest` reads
  ([instance console output](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/troubleshoot-unreachable-instance.html)),
  for `x86_64`, the one architecture `sandboxd` is built for (`lib.rs:275-285`), with the vCPUs
  and memory that the settings state;
- the sandbox instance profile;
- instance metadata with a token required, within one hop, so that no container reaches it;
- a network interface with the sandbox security group, in a subnet that reaches ECR and the
  controller's private address, deleted when the instance terminates;
- EBS volumes deleted when the instance terminates, large enough for the role's image;
- no termination protection: a delete and the timer terminate the instance
  (`launch.rs:437-442`).

## The sandbox AMI

What the user data and `ec2-boot` run, none of which the provider installs or checks before a
launch:

- `x86_64`, with the kernel's console on the serial port, which the boot's unit writes to
  (`StandardOutput=tty`, `providers/ec2/src/pumpkin-ec2-boot.service:19-21`) and the console's
  reader filters by prefix (`crates/schema/src/ec2_boot.rs:137-142`).
- cloud-init that takes a MIME multipart user data (`ec2_boot.rs:77-130`), writes a cloud-config's
  `write_files` in its init stage, runs its `runcmd` in its config stage once per instance
  (`providers/ec2/src/user_data.rs:6-9`), and keeps the user data at
  `/var/lib/cloud/instance/user-data.txt` (`ec2_boot.rs:30`), where `ec2-boot` reads its start.
- systemd, which takes units in `/etc/systemd/system` and `/run/systemd/system`
  (`user_data.rs:75-81`) and `SuccessAction=`/`FailureAction=poweroff`; logind, whose
  `ScheduleShutdown` the unit and `ec2-boot` call through `busctl`
  (`pumpkin-ec2-boot.service:24-26`, `providers/ec2/boot/src/main.rs:160-178`); and
  `/usr/bin/echo`, `/usr/bin/busctl`, `/usr/bin/systemctl` and `/usr/bin/docker` at those paths
  (`pumpkin-ec2-boot.service`).
- Docker as `docker.service`, which the unit starts (`pumpkin-ec2-boot.service:34`), on cgroup v2
  with the memory, swap, CPU and process limits (`main.rs:203-220`), its default bridge named
  `docker0` (`main.rs:186-197`), and `--cgroupns=private` (`main.rs:265-268`).
- `nft` with the kernel's nf_tables, `inet` family (`main.rs:180-201`).
- ECR's credential helper, `docker-credential-ecr-login`, on root's `PATH` and configured by no
  file of the AMI's: the user data writes root's `~/.docker/config.json` (`user_data.rs:67-80`),
  and a helper the AMI configured would break the `docker login` that older releases' boots run.
- A writable `/usr/local/sbin` and `/opt` (`pumpkin-ec2-boot.service:37`, `main.rs:34`).

Root's processes reach the instance's metadata for the role's credentials; the boot's rules drop
everything from `docker0` but the gateway's address and port (`main.rs:180-201`).

## Network

- **The gateway:** each job sets `[gateway] private` to the controller's private IPv4 address at
  port 0 (`crates/pumpkin/src/job.rs:388-389`), so the gateway listens on a port from the
  controller kernel's ephemeral range, Linux's default 32768-60999. A sandbox's container dials
  it at an IPv4 address (`providers/ec2/src/launch.rs:70-77`): the sandboxes' egress and the
  controller's ingress admit TCP on that range between them.
- **Pulls:** a sandbox instance reaches ECR's API, its registry and the S3 that serves the layers,
  over HTTPS.
- **The controller's addresses:** a job reads `local-ipv4` and `public-ipv4` from the instance's
  metadata under an IMDSv2 token (`job.rs:469-492`), so the controller has a public IPv4 address.
  The public one is the Modal provider's `dials_from`, the one address from which its sandboxes'
  tunnels take connections (`job.rs:416-418`).
- **The controller's way out:** HTTPS to AWS's APIs, Modal, the model upstreams and what builds
  fetch.
- **The client's way in:** TCP port 22 at the controller's private address
  (`crates/pumpkin/src/controller/aws.rs:269-272`), so the client is in the controller's VPC or
  routed to it.

## The controller's host

The instance that the client finds by its tag, and its software:

- **The user `pumpkin`**, whom the client logs in as (`crates/pumpkin/src/controller.rs:35`,
  `:199-203`), whose login shell runs the POSIX shell lines that the client sends
  (`crates/pumpkin/src/link.rs:586-599`, `controller/aws.rs:289-292`), and who can use the
  Docker engine of the settings' `[docker] context`.
- **A user manager that outlives sessions:** each job is a unit of `pumpkin`'s, started with
  `systemd-run --user` (`crates/pumpkin/src/job/start.rs:118-150`), named `pumpkin-job-<id>`
  (`crates/pumpkin/src/job.rs:64-65`), which lasts as long as its run, so `pumpkin` lingers.
- **`sshd`:** that takes the key that the client pushes through EC2 Instance Connect, then the one
  it appends to `~pumpkin/.ssh/authorized_keys` (`controller.rs:182-188`,
  `controller/aws.rs:286-313`), and several connections at once: a send shares a task's upload
  across up to four (`crates/pumpkin/src/controller/send.rs`), and a probe past `MaxStartups`
  reads as a refusal (`controller/aws.rs:263-268`).
- **The boot's reap:** the system unit `pumpkin-boot.service`, whose state and `~pumpkin/boot.log`
  the controller reports to the client (`crates/pumpkin/src/serve.rs:52`, `:342-365`), runs each
  install's `~pumpkin/.pumpkin/<digest>/bin/pumpkin-boot controller boot`, a link that the job's
  install makes (`job/start.rs:269-272`).
- **Programs on `PATH`:** AWS CLI v2 (`crates/aws/src/lib.rs:169-173`); Docker with Buildx and the
  containerd image store, which keeps a pushed image's manifest (`aws/src/lib.rs:314-318`);
  `curl` (`job.rs:495-511`); `df` (`job.rs:922`); `systemd-run` and `systemctl`; `uv`, for the
  Modal helper (`providers/modal/src/helper.rs:161`); GNU coreutils' `mkdir --parents`
  (`controller/aws.rs:291`).
- **One volume at `/`** that holds the engine's images and build cache and `pumpkin`'s home
  (`job.rs:99-101`), which a job prunes as it fills.
- **Instance metadata** with a token, reached by the host's processes for the role's credentials
  and the addresses (`job.rs:106-112`, `:469-492`), and not by a build's containers.
- **A stop on shutdown that keeps the volume:** the client starts a stopped controller
  (`controller/aws.rs:142-174`); a terminated one is gone, with its records.
