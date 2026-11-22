//! The examples that end each command's help, below its options, with the exit statuses of the
//! commands that depart from those of `pumpkin --help`, and the topics that `pumpkin help` prints.
//! Each heading has clap's header style. Each exit status has the literal style, as in rustup's
//! help:
//! <https://github.com/rust-lang/rustup/blob/08f60b0a9f445bf25153692139700a2cbc1a869b/src/cli/help.rs#L219-L226>.

use super::{HEADER, LITERAL};

pub(super) fn check_notes() -> String {
    format!(
        "\
{HEADER}Examples:{HEADER:#}
  pumpkin check tasks/bench/task.star --arg model=model-a
  pumpkin check tasks/*/task.star --arg model=model-a

The lint's rules: https://github.com/FrontierCS/Pumpkin/blob/main/docs/language.md#formatting-and-the-lint"
    )
}

pub(super) fn fmt_notes() -> String {
    format!(
        "\
{HEADER}Examples:{HEADER:#}
  pumpkin fmt
  pumpkin fmt --check lib tasks/bench/task.star"
    )
}

pub(super) fn run_notes() -> String {
    format!(
        "\
{HEADER}Examples:{HEADER:#}
  pumpkin run tasks/bench/task.star --arg model=model-a
  pumpkin run tasks/bench/task.star --arg model=model-a --wait --outcome \\
    > outcome.json

How a run is placed and how it outlives a logout:
https://github.com/FrontierCS/Pumpkin/blob/main/docs/running.md#running"
    )
}

pub(super) fn wait_notes() -> String {
    format!(
        "\
{HEADER}Examples:{HEADER:#}
  pumpkin wait 20261108-000905-126a21e9
  pumpkin wait 126a
  pumpkin wait runs/20261108-000905-126a21e9
  pumpkin wait --outcome 126a > outcome.json

{HEADER}Exit status:{HEADER:#}
  {LITERAL}0{LITERAL:#}  The run finished with a result.
  {LITERAL}1{LITERAL:#}  The run failed, was interrupted or was lost, or another error.
  {LITERAL}3{LITERAL:#}  The run finished without a result."
    )
}

pub(super) fn ls_notes() -> String {
    format!(
        "\
{HEADER}Examples:{HEADER:#}
  pumpkin ls
  pumpkin ls 20261110-1
  pumpkin ls --runs runs

The phases and states: https://github.com/FrontierCS/Pumpkin/blob/main/docs/running.md#following-and-listing-runs"
    )
}

pub(super) fn cancel_notes() -> String {
    format!(
        "\
{HEADER}Examples:{HEADER:#}
  pumpkin cancel 126a

{HEADER}Exit status:{HEADER:#}
  {LITERAL}0{LITERAL:#}  The run has ended, and nothing that it left remains.
  {LITERAL}3{LITERAL:#}  The run has ended, but one of its providers can't be reached.
     What the run left there may remain."
    )
}

pub(super) fn view_notes() -> String {
    format!(
        "\
{HEADER}Examples:{HEADER:#}
  pumpkin view
  pumpkin view --runs runs --port 8080

`view` serves until a signal stops it, so it never exits 0.

What the page shows: https://github.com/FrontierCS/Pumpkin/blob/main/docs/record.md#the-page"
    )
}

pub(super) fn help_notes() -> String {
    format!(
        "\
{HEADER}Examples:{HEADER:#}
  pumpkin help run
  pumpkin help config"
    )
}

pub(super) fn reap_notes() -> String {
    format!(
        "\
{HEADER}Examples:{HEADER:#}
  pumpkin reap
  pumpkin reap --runs runs

{HEADER}Exit status:{HEADER:#}
  {LITERAL}0{LITERAL:#}  Nothing that the runs left remains.
  {LITERAL}3{LITERAL:#}  `reap` can't reach a provider of a run, which it names, and nothing else
     failed. What the run left there may remain. The provider's Docker context
     may be gone, or the configuration may no longer name the provider."
    )
}

pub(super) fn cancel_for_notes() -> String {
    format!(
        "\
{HEADER}Exit status:{HEADER:#}
  {LITERAL}102{LITERAL:#}  The run has ended. The client copies its record.
  {LITERAL}103{LITERAL:#}  The run has ended, but one of its providers can't be reached."
    )
}

pub(super) fn controller_notes() -> String {
    format!(
        "\
{HEADER}Examples:{HEADER:#}
  cd ~/trees/tasks/20261108-000905-126a21e9 && \\
    pumpkin controller job 20261108-000905-126a21e9 --region us-west-1 \\
    --parameter /pumpkin-alice/settings \\
    --tree 5d0c5f0e83a3c40be3e7f2b4d1f9a2b6c8e7d6a5b4c3d2e1f0a9b8c7d6e5f4a3 \\
    -- run tasks/bench/task.star --arg model=model-a
  pumpkin controller boot"
    )
}

pub(super) const CONFIG: &str = "\
pumpkin.toml is the machine's configuration, which `pumpkin run` reads. It is
the file that --config names, by default $HOME/.config/pumpkin/pumpkin.toml. It
is TOML, with the tables below. Every key is required unless it says what its
absence means. [gateway] and the [upstreams.<name>] tables may be left out. Any
other key is refused. On a machine whose runs go to the person's controller,
[controller] takes the place of [docker], [providers.<name>.<kind>] and
[gateway].

Example, of a machine whose runs go to EC2 and Modal:
  [docker]
  context = \"default\"

  [providers.aws.ec2]
  region = \"us-west-1\"
  repositories = { roles = \"pumpkin/roles\", sandboxd = \"pumpkin/sandboxd\" }

  [providers.aws.ec2.timer]
  group = \"pumpkin\"
  role_arn = \"arn:aws:iam::123456789012:role/pumpkin-timer\"

  [providers.aws.ec2.classes.cpu]
  template = \"lt-0123456789abcdef0\"
  version = 1
  vcpus = 8
  memory_mib = 16384
  cpu_features = [\"avx2\"]

  [providers.gpus.modal]
  profile = \"pumpkin\"
  dials_from = \"203.0.113.7\"

  [providers.gpus.modal.images]
  region = \"us-west-1\"
  repository = \"pumpkin/roles\"
  role_arn = \"arn:aws:iam::123456789012:role/pumpkin-modal-pull\"

  [providers.gpus.modal.classes.a100]
  gpu = \"A100-80GB\"
  gpu_model = \"NVIDIA A100-SXM4-80GB\"
  cpu_millis = 8000
  memory_bytes = 68719476736
  startup_seconds = 1800

  [gateway]
  private = \"172.31.0.10:7433\"

  [upstreams.gateway]
  url = \"https://gateway.example/v1\"
  key_file = \"gateway-key\"

  [[upstreams.gateway.models]]
  name = \"model-a\"
  upstream_name = \"provider/model-a\"
  usd_per_mtok = { input = 1.25, cached = 0.125, output = 10 }

Example, of a machine whose runs go to the person's controller, and Modal:
  [controller]
  name = \"alice\"
  region = \"us-west-1\"

  [controller.modal]
  profile = \"pumpkin\"

  [controller.modal.classes.a100]
  gpu = \"A100-80GB\"
  gpu_model = \"NVIDIA A100-SXM4-80GB\"
  cpu_millis = 8000
  memory_bytes = 68719476736
  startup_seconds = 1800

[docker] is the machine's Docker engine:
  context       The context that reaches it, as `docker context ls` lists it.

[providers.<name>.<kind>] is a provider that the machine's runs may place roles
on. <name> names it, and <kind> is its kind. Its table holds the kind's
settings:
  docker        Sandboxes on the engine of [docker]. This kind takes no
                settings. A docker provider must be the configuration's only
                provider: a machine runs its roles either on its own engine or
                on providers of kind ec2 and modal. Its one class,
                `container`, offers the machine's online CPUs and its
                memory. It offers the CPU flags that /proc/cpuinfo
                lists for every CPU, no CPU model and no GPU.
  ec2           Sandboxes on EC2 instances of their own, in the envelope that
                its settings name. The instances pull their images from the
                envelope, which the engine of [docker] pushes them to. They
                dial the private address of [gateway]. The provider runs AWS
                CLI v2 as `aws` from PATH. It uses the credentials that `aws`
                finds, such as the instance's role. Its settings:
    region        The envelope's AWS region, which every call names.
    classes       The classes of sandbox, by name. There is at least one. Each
                  class has these keys:
                  `template` is the ID of its launch template, and `version`
                  the template's version that its instances launch from.
                  `vcpus` and `memory_mib` are its instance type's CPUs and
                  memory in MiB. A role gets all the CPUs, and all the memory
                  but a share that the instance keeps.
                  `cpu_features` are its CPU's flags, and `cpu_model` is its
                  CPU's model, as /proc/cpuinfo names them. Without
                  `cpu_features`, the class states no flag. Without
                  `cpu_model`, it states no model. A class that states a model
                  is offered only to roles that need it.
                  `usd_per_hour` is what an instance costs an hour. With it,
                  `ls` and a run's summary price the class's sandboxes.
                  `startup_seconds` is the most seconds that an instance's
                  start may take, its images' pulls included. Without it, the
                  start may take 1740 seconds.
    repositories  The envelope's ECR repositories: `roles` holds the roles'
                  images, and `sandboxd` the images that hold sandboxd.
    timer         The sandboxes' timer, which terminates each instance at its
                  run's hard end. `group` is its EventBridge Scheduler schedule
                  group. `role_arn` is the ARN of the role that its schedules
                  terminate instances with.
  modal         Sandboxes in a Modal workspace. Modal imports their images from
                an ECR repository, which the engine of [docker] pushes them to.
                Each sandbox listens behind Modal's TLS tunnel, which the run
                dials. The provider runs Modal's helper, pumpkin-modal.py,
                beside `pumpkin`, with `uv` from PATH. It runs `aws`, AWS CLI
                v2, as ec2 does. Its settings:
    profile       The profile of Modal's configuration whose token reaches the
                  workspace.
    config        The absolute path of Modal's configuration file that holds
                  the profile. Without it, the file is ~/.modal.toml.
    dials_from    This machine's public IPv4 address. The sandboxes' tunnels
                  take connections from it alone.
    images        Where Modal imports images from. `region` is an AWS region,
                  and `repository` an ECR repository there. `role_arn` is the
                  ARN of the AWS role that Modal pulls with, which trusts
                  Modal's identity.
    classes       The classes of sandbox, by name. There is at least one. Each
                  class has `cpu_millis`, `memory_bytes` in whole MiB and
                  `startup_seconds`. A class with a GPU has `gpu`, as Modal
                  names one, such as \"H100!\", or \"A100-80GB:2\" for two. It
                  then has `gpu_model`, the model as nvidia-smi names it, such
                  as \"NVIDIA H100 80GB HBM3\". A class with a GPU is offered
                  only to roles that need its GPUs. `usd_per_hour` is what a
                  sandbox costs an hour. With it, `ls` and a run's summary
                  price the class's sandboxes.

[gateway] is where the gateway meets the sandboxes that dial it over a network:
  private       The address, ip:port, that the sandboxes of an ec2 provider
                dial. It is one of this machine's addresses on the envelope's
                network, at a port that the envelope's security group admits.
                A run listens there while it places a role on such a provider,
                so only one such run can go at a time.

[upstreams.<name>] is an upstream that a workflow's model route may reach:
  url           The base URL of its API. It is https, or http to a loopback IP
                address, such as 127.0.0.1 but not localhost. The route
                reaches each API's endpoint below it: /responses,
                /chat/completions, or /messages, which an Anthropic Messages
                API serves, such as https://api.anthropic.com/v1.
  key_file      The file that holds its key, below this file's directory unless
                the path is absolute. The route sends the key as
                `authorization: Bearer <key>`, and to /messages as
                `x-api-key: <key>`.
  models        The models it serves. Each has the `name` that a route allows
                and the `upstream_name` that the upstream takes for it.
                `usd_per_mtok` is optional. It says what the model's tokens
                cost in US dollars a million: `input` and `output`, and
                `cached` and `cache_write`, the input that the upstream read
                from its cache and wrote to it. Reasoning counts as output. A
                model without `usd_per_mtok` has no cost. Neither has a model
                whose responses report a kind of token that its prices lack.
                No model is listed twice, under one upstream or two.

Each entry of a workflow's model route reaches the one upstream that serves
every model it allows.

[controller] is the person's controller, which deploy/aws makes. `run`, `wait`,
`ls`, `cancel`, `view` and `reap` go to it, as each one's help says.
Each run is sent the [upstreams.<name>] tables, their keys, [controller.modal]
and Modal's configuration file. They go into a directory of the run's own in
the controller's tmpfs, which the run's end removes. The job runs the client's
install with the module's settings and [controller.modal].
The client uses AWS CLI v2, ssh, ssh-keygen and git from PATH. It sends the
task's files and starts its job over SSH to the controller's private address.
It uses up to four connections for large task uploads. The machine needs a
route there, and deploy/aws's `ssh_from` must admit it. The first command
creates a key and enrols it through EC2 Instance Connect. The client keeps that
key and the pinned host key in $HOME/.config/pumpkin/ssh/.
  name          The `name` that deploy/aws was applied with.
  region        The region it was applied in.

[controller.modal] is the Modal provider that the controller's runs may place
roles on, if any. It takes the settings of a provider of kind modal, but
`images`, which the module's settings give, and `dials_from`, which each run
sets:
  profile       As modal's. The profile is in Modal's configuration file,
                ~/.modal.toml. Each run is sent that file if it is there.
  classes       As modal's.
";
