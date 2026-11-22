//! `cargo xtask live join`: a new member's machine joins the controller. The machine is an EC2
//! instance in the controller's subnet, with the members' instance profile that `deploy/aws`
//! makes, and runs Amazon Linux 2023, which holds the AWS CLI v2 and the OpenSSH that the client
//! uses. Its configuration is the `[controller]` table alone, and its key is this machine's,
//! which `members` lists. The check reaches it over `ssh` from the VPC through a security group
//! of its own. It copies Pumpkin at `HEAD` and the key there, and runs `pumpkin ls`. Then it
//! removes the key, terminates the machine and deletes the group.
//!
//! The key goes over `ssh` alone. The user data holds only the public key that admits the check,
//! as any principal that may describe the instance reads it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};
use std::{fs, thread};

use serde_json::{Value, json};
use stdx::process::{self, Bound, Echo, Environment, Run, Stream};

use super::{PUMPKIN_TIMEOUT, aws, head_commit, identified, kept, listed, refused};
use crate::repo::STEP_TIMEOUT;
use crate::settings::{Provider, Settings};
use crate::{Result, target_dir};

/// The tag of the machine and of its group, valued with the check's id. The contributor's policy
/// lets it end only what carries the tag.
const TAG: &str = "pumpkin:join";

/// The machine's instance type, the one that the contributor's policy allows. `pumpkin ls` needs
/// little of it.
const INSTANCE_TYPE: &str = "t3.micro";

/// What a refusal of a call that the contributor's policy allows says to do.
const POLICY: &str = "grant these credentials the policy that `deploy/aws` outputs as \
                      `contributor_policy_arn`, at this commit, and run this again";

/// The names of Amazon Linux 2023's standard AMIs on x86-64, of which the newest is taken, as
/// Packer's `most_recent` takes it:
/// <https://github.com/hashicorp/packer/blob/0ca418d656cd5c28798ed5accdd132680979a6de/builder/amazon/common/step_source_ami_info.go#L39-L44>.
const AMI_NAMES: &str = "al2023-ami-2023.*-x86_64";

/// The user of Amazon Linux 2023 whose key cloud-init sets from the user data.
const USER: &str = "ec2-user";

/// How long the machine has, from its launch, to answer `ssh`. An instance boots within a few
/// minutes. Past this, something else is wrong.
const READY_TIMEOUT: Duration = Duration::from_mins(10);

const READY_POLL: Duration = Duration::from_secs(5);

/// How long a probe of `ssh` waits to connect.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the machine lives, from its boot, if the check never ends it: then it powers off,
/// which terminates it.
const LIFETIME: Duration = Duration::from_hours(3);

// The machine outlives the check's steps: its boot, the four commands that copy, `pumpkin ls`, its
// check and the removal of the key.
const _: () = assert!(
    READY_TIMEOUT.as_secs() + 6 * STEP_TIMEOUT.as_secs() + PUMPKIN_TIMEOUT.as_secs()
        < LIFETIME.as_secs()
);

/// How often, and how long apart, a deletion of the group is tried. EC2 refuses it while the
/// terminated instance's interface still holds it. Packer's:
/// <https://github.com/hashicorp/packer/blob/0ca418d656cd5c28798ed5accdd132680979a6de/builder/amazon/common/step_security_group.go#L173-L181>.
const DELETE_TRIES: usize = 5;

const DELETE_POLL: Duration = Duration::from_secs(5);

/// Runs the check, and fails unless `pumpkin ls` passed on the new machine and nothing of the
/// check is left.
pub(crate) fn join() -> Result<()> {
    // The settings of a smoke run on EC2: the controller's name and its region.
    let settings = super::configured(Provider::Ec2, None)?;
    identified(&settings)?;
    let kept = kept()?;
    for file in ["key", "key.pub"] {
        if !kept.join(file).exists() {
            return Err(format!(
                "{} holds no `{file}`, the key that the new machine joins with: run `./pumpkin \
                 ls` here once, have the admin list its key, and run this again",
                kept.display()
            )
            .into());
        }
    }
    let commit = head_commit()?;
    let place = target_dir()?.join("live/join");
    let workspace = place.join(".pumpkin");
    crate::build(Some(workspace.clone()), crate::MUSL)?;
    // Mode 0700, as `tempfile` makes it, as it holds the key that reaches the machine.
    let scratch = tempfile::Builder::new()
        .prefix("scratch-")
        .tempdir_in(&place)
        .map_err(|error| format!("making a directory in {} failed: {error}", place.display()))?;
    let reach = Reach::new(scratch.path())?;
    let config = scratch.path().join("pumpkin.toml");
    fs::write(&config, controller_table(&settings))
        .map_err(|error| format!("writing {} failed: {error}", config.display()))?;
    let id = format!("join-{}", unix_seconds()?);
    let copied = Copied {
        workspace: &workspace,
        config: &config,
        kept: &kept,
    };
    let mut made = Made::default();
    let joined = joined(&settings, &id, &reach, &copied, &mut made);
    let left = ended(&settings, &id, &reach, &made);
    let mut failed = Vec::new();
    if let Err(error) = joined {
        failed.push(error.to_string());
    }
    if !left.is_empty() {
        failed.push(format!(
            "left after the check: {}: remove each with `aws ec2 terminate-instances` and `aws \
             ec2 delete-security-group`",
            left.join(", ")
        ));
    }
    if failed.is_empty() {
        println!(
            "xtask: a new member's machine ran `pumpkin ls` at {commit}, and nothing of it is left"
        );
        return Ok(());
    }
    Err(format!(
        "{}: fix it, and run `cargo xtask live join` again",
        failed.join("; ")
    )
    .into())
}

/// What the check made, which it ends whatever else happens.
#[derive(Default)]
struct Made {
    group: Option<String>,
    machine: Option<Machine>,
    /// Whether the machine answered `ssh`, and so may hold the key.
    reached: bool,
}

#[derive(Clone)]
struct Machine {
    id: String,
    address: String,
}

/// What a member's machine holds: the install in `workspace`, the configuration `config`, and
/// the key in `kept`.
struct Copied<'a> {
    workspace: &'a Path,
    config: &'a Path,
    kept: &'a Path,
}

/// Launches the machine, copies `copied` there, and runs `pumpkin ls` there. Each thing it makes
/// goes into `made`.
fn joined(
    settings: &Settings,
    id: &str,
    reach: &Reach,
    copied: &Copied<'_>,
    made: &mut Made,
) -> Result<()> {
    let (subnet, vpc) = controller_subnet(settings)?;
    let image = newest_image(settings)?;
    let group = made.group.insert(created_group(settings, &vpc, id)?);
    // EC2 may not yet know a group it has just made, as Packer waits for it:
    // https://github.com/hashicorp/packer/blob/0ca418d656cd5c28798ed5accdd132680979a6de/builder/amazon/common/step_security_group.go#L100-L112
    let exists = ["ec2", "wait", "security-group-exists", "--group-ids", group];
    let how = Run::new(Bound::Within(STEP_TIMEOUT), Echo::Silent);
    process::run(&mut aws(settings, &exists), how).map_err(|failure| refused(&failure, None))?;
    // `ssh` from the VPC's blocks, as the controller's group admits it.
    let ranges: Vec<Value> = vpc_blocks(settings, &vpc)?
        .into_iter()
        .map(|block| json!({ "CidrIp": block }))
        .collect();
    let ssh = json!([{ "IpProtocol": "tcp", "FromPort": 22, "ToPort": 22, "IpRanges": ranges }]);
    listed(
        &mut aws(
            settings,
            &[
                "ec2",
                "authorize-security-group-ingress",
                "--group-id",
                group,
                "--ip-permissions",
                &ssh.to_string(),
            ],
        ),
        None,
    )?;
    let machine = launched(settings, id, &image, &subnet, group, reach)?;
    made.machine = Some(machine.clone());
    println!(
        "xtask: machine {} launched in the controller's subnet",
        machine.id
    );
    ready(reach, &machine)?;
    made.reached = true;
    copy(reach, &machine, copied)?;
    println!("xtask: `pumpkin ls` on machine {}:", machine.id);
    let mut ls = reach.ssh(&machine, ".pumpkin/bin/pumpkin ls");
    let how = Run::new(Bound::Within(PUMPKIN_TIMEOUT), Echo::Silent)
        .stdout(Stream::Inherited)
        .stderr(Stream::Inherited)
        .named("pumpkin ls");
    process::run(&mut ls, how).map_err(|failure| {
        format!(
            "{} on the new machine, as it said above",
            failure.unadvised()
        )
    })?;
    // `pumpkin ls` pins the controller's host key once it reaches its `sshd`. Without a
    // `[controller]` it would list the machine's own runs instead.
    let pinned = "test -s .config/pumpkin/ssh/known_hosts";
    process::run(&mut reach.ssh(&machine, pinned), step().named("ssh")).map_err(|_| {
        "`pumpkin ls` passed on the new machine, but pinned no host key, so it didn't reach the \
         controller: expected it to go there, as the machine's `[controller]` names it"
    })?;
    Ok(())
}

/// The tags of a resource of `kind` that the check with id `id` makes, as
/// `--tag-specifications` takes them.
fn tags(kind: &str, id: &str) -> Value {
    json!({ "ResourceType": kind, "Tags": [{ "Key": TAG, "Value": id }] })
}

/// Creates the security group of the check with id `id` in VPC `vpc`, which admits nothing yet,
/// and returns its id.
fn created_group(settings: &Settings, vpc: &str, id: &str) -> Result<String> {
    let name = format!("pumpkin-{}-{id}", settings.name);
    let created = listed(
        &mut aws(
            settings,
            &[
                "ec2",
                "create-security-group",
                "--group-name",
                &name,
                "--description",
                "A machine of cargo xtask live join: ssh from the VPC",
                "--vpc-id",
                vpc,
                "--tag-specifications",
                &json!([tags("security-group", id)]).to_string(),
            ],
        ),
        Some(POLICY),
    )?;
    match created["GroupId"].as_str() {
        Some(group) => Ok(group.to_owned()),
        None => Err(format!("`aws ec2 create-security-group` printed no group: {created}").into()),
    }
}

/// Launches the machine of the check with id `id` from AMI `image` in `subnet` and `group`,
/// with the members' instance profile.
fn launched(
    settings: &Settings,
    id: &str,
    image: &str,
    subnet: &str,
    group: &str,
    reach: &Reach,
) -> Result<Machine> {
    let profile = json!({ "Name": format!("pumpkin-{}-member", settings.name) });
    let launched = listed(
        &mut aws(
            settings,
            &[
                "ec2",
                "run-instances",
                "--image-id",
                image,
                "--instance-type",
                INSTANCE_TYPE,
                "--subnet-id",
                subnet,
                "--security-group-ids",
                group,
                "--iam-instance-profile",
                &profile.to_string(),
                "--instance-initiated-shutdown-behavior",
                "terminate",
                "--user-data",
                &user_data(&reach.public_key()?),
                "--tag-specifications",
                &json!([tags("instance", id), tags("volume", id)]).to_string(),
            ],
        ),
        None,
    )?;
    let instance = &launched["Instances"][0];
    match (
        instance["InstanceId"].as_str(),
        instance["PrivateIpAddress"].as_str(),
    ) {
        (Some(instance), Some(address)) => Ok(Machine {
            id: instance.to_owned(),
            address: address.to_owned(),
        }),
        _ => Err(format!(
            "`aws ec2 run-instances` printed no instance with a private address: {launched}"
        )
        .into()),
    }
}

/// The subnet and the VPC of the controller of `settings`, which `deploy/aws` tags with its name.
fn controller_subnet(settings: &Settings) -> Result<(String, String)> {
    let filter = format!("Name=tag:pumpkin:controller,Values={}", settings.name);
    let states = "Name=instance-state-name,Values=pending,running,stopping,stopped";
    let described = listed(
        &mut aws(
            settings,
            &["ec2", "describe-instances", "--filters", &filter, states],
        ),
        None,
    )?;
    let instances: Vec<&Value> = (described["Reservations"].as_array().into_iter().flatten())
        .flat_map(|reservation| reservation["Instances"].as_array().into_iter().flatten())
        .collect();
    let [instance] = instances[..] else {
        return Err(format!(
            "{} instances in {} carry the tag pumpkin:controller={}: expected the one controller \
             that `deploy/aws` made",
            instances.len(),
            settings.region,
            settings.name
        )
        .into());
    };
    match (instance["SubnetId"].as_str(), instance["VpcId"].as_str()) {
        (Some(subnet), Some(vpc)) => Ok((subnet.to_owned(), vpc.to_owned())),
        _ => Err(format!("the controller's instance names no subnet and VPC: {instance}").into()),
    }
}

/// The newest of Amazon Linux 2023's AMIs in the controller's region.
fn newest_image(settings: &Settings) -> Result<String> {
    let name = format!("Name=name,Values={AMI_NAMES}");
    let described = listed(
        &mut aws(
            settings,
            &[
                "ec2",
                "describe-images",
                "--owners",
                "amazon",
                "--filters",
                &name,
                "Name=architecture,Values=x86_64",
                "Name=state,Values=available",
            ],
        ),
        Some(POLICY),
    )?;
    let images = described["Images"].as_array().into_iter().flatten();
    // RFC 3339 times in UTC, which sort as text.
    let newest = images.max_by_key(|image| image["CreationDate"].as_str().unwrap_or_default());
    match newest.and_then(|image| image["ImageId"].as_str()) {
        Some(image) => Ok(image.to_owned()),
        None => Err(format!(
            "no AMI of Amazon's is named {AMI_NAMES} in {}: expected Amazon Linux 2023's",
            settings.region
        )
        .into()),
    }
}

/// The IPv4 blocks associated with VPC `vpc`.
fn vpc_blocks(settings: &Settings, vpc: &str) -> Result<Vec<String>> {
    let described = listed(
        &mut aws(settings, &["ec2", "describe-vpcs", "--vpc-ids", vpc]),
        None,
    )?;
    let associations = described["Vpcs"][0]["CidrBlockAssociationSet"].as_array();
    let blocks: Vec<String> = (associations.into_iter().flatten())
        .filter(|association| association["CidrBlockState"]["State"] == "associated")
        .filter_map(|association| association["CidrBlock"].as_str().map(str::to_owned))
        .collect();
    if blocks.is_empty() {
        return Err(format!("VPC {vpc} has no associated IPv4 block: {described}").into());
    }
    Ok(blocks)
}

/// The user data of the machine: a cloud-config that admits `key` for [`USER`], and powers the
/// machine off at the end of its [`LIFETIME`].
// JSON, which YAML reads as it is, as the EC2 provider's user data is. cloud-init quotes each word
// of a command's list:
// https://github.com/canonical/cloud-init/blob/905d10891051496bd196798b2a8e0d77f161db74/cloudinit/util.py#L2180-L2203
fn user_data(key: &str) -> String {
    let minutes = (LIFETIME.as_secs() / 60).to_string();
    let config = json!({
        "ssh_authorized_keys": [key],
        "runcmd": [["shutdown", "--poweroff", format!("+{minutes}")]],
    });
    format!("#cloud-config\n{config:#}\n")
}

/// The configuration of a member's machine: the `[controller]` table alone, which names the
/// controller of `settings`.
fn controller_table(settings: &Settings) -> String {
    let mut controller = toml::Table::new();
    controller.insert("name".to_owned(), settings.name.clone().into());
    controller.insert("region".to_owned(), settings.region.clone().into());
    let mut table = toml::Table::new();
    table.insert("controller".to_owned(), controller.into());
    table.to_string()
}

/// Waits until `machine` answers `ssh`.
fn ready(reach: &Reach, machine: &Machine) -> Result<()> {
    let by = Instant::now() + READY_TIMEOUT;
    loop {
        let how = Run::new(Bound::Within(PROBE_TIMEOUT * 2), Echo::Silent).named("ssh");
        let Err(failure) = process::run(&mut reach.ssh(machine, "true"), how) else {
            return Ok(());
        };
        if Instant::now() > by {
            return Err(format!(
                "machine {} didn't answer `ssh` within {} minutes of its launch: {failure}: run \
                 this from a machine in the controller's VPC, which the group admits",
                machine.id,
                READY_TIMEOUT.as_secs() / 60
            )
            .into());
        }
        thread::sleep(READY_POLL);
    }
}

/// Copies `copied` to `machine`, where a member's machine keeps each.
fn copy(reach: &Reach, machine: &Machine, copied: &Copied<'_>) -> Result<()> {
    let Copied {
        workspace,
        config,
        kept,
    } = copied;
    let made = "umask 077 && mkdir --parents .pumpkin .config/pumpkin/ssh";
    let how = || step().named("ssh");
    process::run(&mut reach.ssh(machine, made), how()).map_err(|failure| failure.to_string())?;
    let files: [(Vec<PathBuf>, &str); 3] = [
        (vec![workspace.join("bin")], ".pumpkin/"),
        (vec![config.to_path_buf()], ".config/pumpkin/"),
        (
            vec![kept.join("key"), kept.join("key.pub")],
            ".config/pumpkin/ssh/",
        ),
    ];
    for (from, to) in files {
        let mut scp = reach.scp(machine, &from, to);
        process::run(&mut scp, step().named("scp")).map_err(|failure| failure.to_string())?;
    }
    Ok(())
}

/// Ends what the check with id `id` made: removes the key from the machine, terminates each
/// instance that carries the check's tag, and deletes the group. Returns what is left.
fn ended(settings: &Settings, id: &str, reach: &Reach, made: &Made) -> Vec<String> {
    let mut left = Vec::new();
    if let Some(machine) = &made.machine
        && made.reached
    {
        let removed = "rm --force .config/pumpkin/ssh/key";
        let how = step().named("ssh");
        if let Err(failure) = process::run(&mut reach.ssh(machine, removed), how) {
            // Its termination deletes its volume, and the key with it.
            eprintln!(
                "xtask: the key wasn't removed from machine {}: {failure}",
                machine.id
            );
        }
    }
    // By its tag, so that an instance whose launch said nothing that was read ends too.
    let instances = match tagged_instances(settings, id) {
        Ok(instances) => instances,
        Err(error) => {
            eprintln!("xtask: {error}");
            let recorded = made.machine.iter().map(|machine| machine.id.clone());
            left.extend(recorded.map(|machine| format!("EC2 instance {machine}")));
            Vec::new()
        }
    };
    if !instances.is_empty() {
        let mut terminate = vec!["ec2", "terminate-instances", "--instance-ids"];
        terminate.extend(instances.iter().map(String::as_str));
        let terminated = listed(&mut aws(settings, &terminate), None).and_then(|_| {
            // The waiter's own bound is 40 polls, 15 s apart, within this one.
            let mut wait = vec!["ec2", "wait", "instance-terminated", "--instance-ids"];
            wait.extend(instances.iter().map(String::as_str));
            let how = Run::new(Bound::Within(STEP_TIMEOUT * 2), Echo::Silent);
            match process::run(&mut aws(settings, &wait), how) {
                Ok(_) => Ok(()),
                Err(failure) => Err(refused(&failure, None).into()),
            }
        });
        match terminated {
            Ok(()) => println!("xtask: machine {} is terminated", instances.join(", ")),
            Err(error) => {
                eprintln!("xtask: {error}");
                left.extend(
                    instances
                        .iter()
                        .map(|machine| format!("EC2 instance {machine}")),
                );
            }
        }
    }
    let Some(group) = &made.group else {
        return left;
    };
    if !left.is_empty() {
        // The machine holds the group until it is terminated.
        left.push(format!("security group {group}"));
        return left;
    }
    let delete = ["ec2", "delete-security-group", "--group-id", group];
    for tried in 1..=DELETE_TRIES {
        let how = Run::new(Bound::Within(STEP_TIMEOUT), Echo::Silent);
        match process::run(&mut aws(settings, &delete), how) {
            Ok(_) => {
                println!("xtask: security group {group} is deleted");
                return left;
            }
            Err(failure) if tried == DELETE_TRIES => {
                eprintln!("xtask: {}", refused(&failure, None));
            }
            Err(_) => thread::sleep(DELETE_POLL),
        }
    }
    left.push(format!("security group {group}"));
    left
}

/// The instances that carry the tag of the check with id `id` and aren't terminated.
fn tagged_instances(settings: &Settings, id: &str) -> Result<Vec<String>> {
    let filter = format!("Name=tag:{TAG},Values={id}");
    let states = "Name=instance-state-name,Values=pending,running,shutting-down,stopping,stopped";
    let described = listed(
        &mut aws(
            settings,
            &["ec2", "describe-instances", "--filters", &filter, states],
        ),
        None,
    )?;
    let instances = (described["Reservations"].as_array().into_iter().flatten())
        .flat_map(|reservation| reservation["Instances"].as_array().into_iter().flatten())
        .filter_map(|instance| instance["InstanceId"].as_str().map(str::to_owned));
    Ok(instances.collect())
}

/// How the check reaches the machine over `ssh`: a key of its own, which the user data admits,
/// and the host key that the machine shows first, which it then pins.
// The host key is taken on first use, inside the VPC. Packer's communicator takes any:
// https://github.com/hashicorp/packer/blob/0ca418d656cd5c28798ed5accdd132680979a6de/helper/communicator/config.go#L257-L264
struct Reach {
    key: PathBuf,
    known_hosts: PathBuf,
}

impl Reach {
    /// Makes the key in `scratch`, which also holds the pinned host key.
    fn new(scratch: &Path) -> Result<Self> {
        let key = scratch.join("reach");
        let mut keygen = process::command("ssh-keygen", Environment::Only(&["PATH"]));
        keygen
            .args([
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                "cargo-xtask-live-join",
                "-f",
            ])
            .arg(&key);
        process::run(
            &mut keygen,
            Run::new(Bound::Within(STEP_TIMEOUT), Echo::Silent),
        )
        .map_err(|failure| failure.to_string())?;
        Ok(Self {
            key,
            known_hosts: scratch.join("known_hosts"),
        })
    }

    fn public_key(&self) -> Result<String> {
        let public = self.key.with_extension("pub");
        let text = fs::read_to_string(&public)
            .map_err(|error| format!("reading {} failed: {error}", public.display()))?;
        Ok(text.trim().to_owned())
    }

    /// An `ssh` that runs `command` on `machine`.
    fn ssh(&self, machine: &Machine, command: &str) -> Command {
        let mut ssh = self.command("ssh");
        ssh.arg(format!("{USER}@{}", machine.address)).arg(command);
        ssh
    }

    /// An `scp` that copies `from`, each file or tree, into `to`, below the home of [`USER`] on
    /// `machine`, with their modes.
    fn scp(&self, machine: &Machine, from: &[PathBuf], to: &str) -> Command {
        let mut scp = self.command("scp");
        scp.args(["-p", "-r"])
            .args(from)
            .arg(format!("{USER}@{}:{to}", machine.address));
        scp
    }

    fn command(&self, program: &str) -> Command {
        let known_hosts = format!("UserKnownHostsFile={}", self.known_hosts.display());
        let connect = format!("ConnectTimeout={}", PROBE_TIMEOUT.as_secs());
        let mut command = process::command(program, Environment::Only(&["HOME", "PATH"]));
        command
            .args(["-F", "/dev/null", "-i"])
            .arg(&self.key)
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                &known_hosts,
                "-o",
                &connect,
            ]);
        command
    }
}

/// A step that copies to the machine or runs there, echoed with its output.
fn step() -> Run<'static> {
    crate::step(STEP_TIMEOUT)
}

fn unix_seconds() -> Result<u64> {
    let since = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH);
    Ok(since
        .map_err(|error| format!("the clock is before 1970: {error}"))?
        .as_secs())
}
