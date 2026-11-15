//! A sandbox's instance: its launch, from the launch template of its class, and the boot that its
//! user data runs ([`ENTRY`], then `ec2-boot`), which starts the role's container under the
//! [confinement](#confinement); its timer, as `Timer` states it; then its run's list, which finds
//! it, its termination, and its serial console.
//!
//! # Confinement
//!
//! A sandbox's container has the Docker provider's [`confinement`](engine::confinement), and
//! `sandboxd`, and `sandboxctl` for a role that makes ops, mounted read-only from the instance.
//! It reaches the gateway's address and port, and nothing else: neither its instance, nor the
//! instance's metadata, nor any other address, over IPv4 or IPv6; its DNS server is its own
//! loopback, where nothing answers. Only `sandboxd` gets the grant, in its environment, and
//! nothing else runs in the container until `sandboxd` has presented it. The instance's Docker
//! holds a login to the registry, by the instance profile's credentials, only while it pulls the
//! images.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::time::Duration;
use std::{fmt, process, thread};

use aws::Bound;
use engine::confinement;
use provider::{Console, Create, Created, Listed, Role, RunTag, SandboxId, Sweep, UnixTime};
use schema::ec2_boot::{BOOT_IN_IMAGE, CONSOLE_PREFIX, GRANT_DIR, Start};
use schema::ir::Name;
use schema::sandbox::TcpDial;
use serde::Deserialize;
use serde_json::{Value, json};
use stdx::{Context as _, Deadline, Utc};

use crate::{Ec2, InstanceClass, Reached};

/// The entry, which a sandbox's user data is once [`user_data`] fills in its values.
const ENTRY: &str = include_str!("entry.sh");

/// The tag that carries an instance's run, by which its run's list finds it.
const RUN_TAG: &str = "pumpkin:run";

const ROLE_TAG: &str = "pumpkin:role";

// EC2's API is eventually consistent, so an instance just launched may be missing from a list
// for a while; one still missing minutes later is lost, not late. This is the usual bound of a
// wait for a new instance to be listed.
const LISTED_TIMEOUT: Duration = Duration::from_secs(200);

// An instance ends some time after it is told to: its OS shuts down, then EC2 frees its volume
// and network interface, which the envelope's templates delete with it. That takes seconds, or
// minutes for a slow shutdown; one still not terminated after 10 min is stuck, not slow.
const TERMINATED_TIMEOUT: Duration = Duration::from_mins(10);

/// How often instances are described again while a create waits for its run's list to find the
/// instance it launched, or a delete for EC2 to describe those it terminated as terminated.
const DESCRIBE_EVERY: Duration = Duration::from_secs(5);

impl Ec2<'_> {
    /// Creates `create`'s sandbox on an instance of its own, with its timer, and returns once its
    /// run's list finds it, reporting `{"ami": a, "timer": {"group": g, "name": n}}`.
    ///
    /// # Errors
    ///
    /// Fails before launching if `dial` isn't to IPv4 or the class isn't the envelope's; fails if
    /// `create.by` passes first, or the list misses the instance for 200 s, leaving any instance
    /// it launched, which carries the run's tag.
    pub(crate) fn create_instance(
        &self,
        create: &Create,
        dial: TcpDial,
    ) -> Result<Created, String> {
        let Create {
            run,
            name,
            role,
            // Each sandbox has an instance of its own, whichever instance of its role it is.
            instance: _,
            class,
            image,
            sandboxd,
            ops,
            model,
            hard_end,
            by,
        } = create;
        let TcpDial { gateway, grant } = dial;
        let SocketAddr::V4(gateway) = gateway else {
            // Docker's default bridge, which the container is on, has no IPv6.
            return Err(format!(
                "an EC2 sandbox's container dials the gateway at an IPv4 address on the \
                 envelope's network: expected one, not {gateway}"
            ));
        };
        let Role {
            name: role_name,
            user,
            resources,
            instances: _,
        } = role;
        let class = (self.classes.get(class))
            .ok_or_else(|| format!("class `{class}` is none of the envelope's"))?;
        let hard_end = HardEnd::of(*hard_end);
        // Times in whole seconds, rounded down, so that neither comes after its own, as the hard
        // end's own type has it.
        let start = Start {
            hard_end_unix_seconds: hard_end.0,
            // The create's own deadline: should the pull end too late for `sandboxd` to meet the
            // gateway by then, the run has stopped waiting, and the gateway refuses it.
            pulls_by_unix_seconds: by.since_epoch().as_secs(),
            registry: self.repositories.roles.registry().to_owned(),
            role_image: image.as_str().to_owned(),
            sandboxd_image: sandboxd.as_str().to_owned(),
            confinement: confinement(*user, *resources),
            gateway,
            grant,
            ops: *ops,
            model: *model,
        };
        let by = Deadline::at_unix(by.since_epoch());
        let tags = tag_specifications(run, name, role_name);
        let (id, ami) = self.launch(
            class,
            run,
            name,
            &tags,
            &user_data(&start, self.region.as_str()),
            by,
        )?;
        let timer = self.set_timer(run, &id, hard_end, by)?;
        let group = &self.reached.group.0;
        self.listed(run, &id, by)?;
        Ok(Created {
            id: SandboxId::from(id.0),
            report: json!({ "ami": ami, "timer": { "group": group, "name": timer } }),
        })
    }

    /// Launches one instance from `class`'s template, at its version, for sandbox `name` of run
    /// `run`, with tags `tags` and user data `user_data`, by `by`, and returns its ID and the ID
    /// of the AMI it launched with.
    fn launch(
        &self,
        class: &InstanceClass,
        run: &RunTag,
        name: &Name,
        tags: &str,
        user_data: &str,
        by: Deadline,
    ) -> Result<(InstanceId, String), String> {
        // The user data holds the grant, so it reaches `aws` in a file only this user can read,
        // never on its command line, which every process can read. Neither a run's tag nor a
        // name holds a `+`, so no other launch of this process names the file.
        let file = self.dir.join(format!(
            "pumpkin-user-data-{}-{}+{name}",
            process::id(),
            run.as_str()
        ));
        let uri = format!("file://{}", file.display());
        let launch_template = format!(
            "LaunchTemplateId={},Version={}",
            class.template.0, class.version
        );
        // No client token: the run makes one create of each sandbox, and `aws` gives each call
        // a token of its own, which keeps its own retries of the call from launching twice.
        let args = [
            "ec2",
            "run-instances",
            "--launch-template",
            &launch_template,
            // A shutdown, as the boot makes at its end and at the hard end, terminates the
            // instance, whatever its template says, so that nothing of it runs on, or is billed.
            "--instance-initiated-shutdown-behavior",
            "terminate",
            "--tag-specifications",
            tags,
            "--user-data",
            &uri,
        ];
        let launched = write_private(&self.dir, &file, user_data)
            .and_then(|()| self.reached.cli.json::<Launched>(&args, Bound::Create(by)));
        // A file left behind holds a grant that only this user can read, and that expires with
        // its registration.
        let _ = fs::remove_file(&file);
        let Launched {
            instances:
                [
                    LaunchedInstance {
                        instance_id,
                        image_id,
                    },
                ],
        } = launched?;
        Ok((instance_id, image_id))
    }

    /// Makes the timer ([`Timer`](crate::Timer)) of instance `id`, of run `run`, which was just
    /// launched, at `hard_end`, by `by`, and returns its name.
    fn set_timer(
        &self,
        run: &RunTag,
        id: &InstanceId,
        hard_end: HardEnd,
        by: Deadline,
    ) -> Result<String, String> {
        let (group, role_arn) = (&self.reached.group, &self.timer_role);
        let name = timer_name(run, id);
        // In UTC, as `--schedule-expression-timezone` says.
        let at = format!("at({})", Utc::from_unix(hard_end.timer()));
        // EventBridge Scheduler's universal target, which makes the call that its ARN names, with
        // the input as its parameters.
        let target = json!({
            "Arn": format!("arn:{}:scheduler:::aws-sdk:ec2:terminateInstances", role_arn.partition()),
            "RoleArn": role_arn.as_str(),
            "Input": json!({ "InstanceIds": [id.0] }).to_string(),
        });
        let target = target.to_string();
        let args = [
            "scheduler",
            "create-schedule",
            "--group-name",
            &group.0,
            "--name",
            &name,
            "--schedule-expression",
            &at,
            "--schedule-expression-timezone",
            "UTC",
            "--flexible-time-window",
            "Mode=OFF",
            "--action-after-completion",
            "DELETE",
            "--target",
            &target,
        ];
        let made = self.reached.cli.run(&args, Bound::Create(by)).map(drop);
        made.context(format_args!(
            "making instance `{id}`'s timer, schedule `{}/{name}`, failed",
            group.0
        ))?;
        Ok(name)
    }

    /// Lists run `run` until its list finds instance `id`, in whatever state, for up to
    /// [`LISTED_TIMEOUT`], and by `by`.
    // A run's list must find each sandbox from when its create returns, and the run takes one
    // that a list misses for ended (`Sweep::list`), so the create returns only once a list has
    // found the instance; one that has ended already is found, and so seen to have ended.
    fn listed(&self, run: &RunTag, id: &InstanceId, by: Deadline) -> Result<(), String> {
        let listing = Deadline::after(LISTED_TIMEOUT);
        let filters = [run_filter(run), ids_filter([id])];
        loop {
            if !(self.reached.described(&filters, Bound::Create(by))?).is_empty() {
                return Ok(());
            }
            let Some(left) = listing.remaining() else {
                return Err(format!(
                    "run `{}`'s list didn't find instance `{id}` within {} s of its launch: \
                     expected EC2 to list it by then",
                    run.as_str(),
                    LISTED_TIMEOUT.as_secs()
                ));
            };
            // Past `by`, the next list fails at once, saying so.
            let before_by = by.remaining().unwrap_or(Duration::ZERO);
            thread::sleep(left.min(before_by).min(DESCRIBE_EVERY));
        }
    }

    /// Reads what instance `id` of run `run` has printed on its serial console, the latest that EC2
    /// shows, of which it keeps the lines its boot wrote there, whose last say how the boot ended
    /// and what its container printed last. EC2 may show a line only tens of seconds after it is
    /// printed, and shows nothing of a terminated instance. It fails if `id` isn't an instance of
    /// run `run` that EC2 describes, such as another run's.
    pub(crate) fn read_console(&self, run: &RunTag, id: &SandboxId) -> Result<Console, String> {
        let not_ours = || {
            format!(
                "`{}` is no sandbox of run `{}`: name one that the run's list finds",
                id.as_str().escape_debug(),
                run.as_str()
            )
        };
        let Ok(id) = InstanceId::try_from(id.as_str().to_owned()) else {
            return Err(not_ours());
        };
        // By ID alone, so that one terminated, which the run's list leaves out, is found too,
        // as a start that has ended reads its console; and kept to the run by its tag.
        let found = (self.reached).described(&[ids_filter([&id])], Bound::Call)?;
        if !(found.iter()).any(|found| found.instance_id == id && found.is_of(run)) {
            return Err(not_ours());
        }
        let read = [
            "ec2",
            "get-console-output",
            "--instance-id",
            &id.0,
            "--latest",
        ];
        let ConsoleOutput { output } = self.reached.cli.json(&read, Bound::Call)?;
        // The boot's own lines alone, which are all it writes there ([`ENTRY`]): the kernel's,
        // cloud-init's and the shutdown's tell nothing of the sandbox.
        let output = output.unwrap_or_default();
        let boot = (output.lines()).filter(|line| line.starts_with(CONSOLE_PREFIX));
        let text = boot.flat_map(|line| [line, "\n"]).collect();
        Ok(Console { text })
    }
}

impl Sweep for Reached {
    /// Lists run `run`'s instances, as `provider::Sweep::list` asks: those its tag finds, but
    /// those terminated, which EC2 has freed, as the [envelope](crate#the-envelope) asks; each
    /// [ended](Listed::ended) unless it is pending or running. Its `aws` call is killed after
    /// 10 min.
    fn list(&self, run: &RunTag) -> Result<Vec<Listed>, String> {
        let found = self.described(&[run_filter(run)], Bound::Call)?;
        let listed = found.into_iter().filter_map(|found| {
            let ListedInstance {
                instance_id,
                state: InstanceState { name },
                tags: _,
            } = found;
            // The boot runs once, and a later boot starts no container, so an instance that is
            // ending, or has stopped, never serves its sandbox again.
            let ended = match name {
                State::Pending | State::Running => false,
                State::ShuttingDown | State::Stopping | State::Stopped => true,
                State::Terminated => return None,
            };
            let id = SandboxId::from(instance_id.0);
            Some(Listed { id, ended })
        });
        Ok(listed.collect())
    }

    /// Deletes run `run`'s instances that `ids` name, as `provider::Sweep::delete` asks: terminates
    /// each that isn't terminated or shutting down already, waits until EC2 describes each as
    /// terminated, or no more, then deletes each one's timer.
    ///
    /// # Errors
    ///
    /// Fails if AWS refuses to describe the instances, terminating none; or, naming each, if one
    /// isn't the run's, which is left, if AWS refuses to terminate it, if it isn't terminated
    /// within the wait, which may leave it running, or if AWS refuses its timer's delete.
    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> Result<(), String> {
        // An id that isn't an instance's names none, and the filter takes only IDs.
        let asked = ids
            .iter()
            .map(|id| InstanceId::try_from(id.as_str().to_owned()));
        let asked: BTreeSet<InstanceId> = asked.filter_map(Result::ok).collect();
        if asked.is_empty() {
            return Ok(());
        }
        let unfound = format_args!(
            "looking up {} failed, so no instance was terminated",
            shown(&asked)
        );
        // By ID alone, so that another run's instance is found, and left, by its tag.
        let found = (self.described(&[ids_filter(&asked)], Bound::Call)).context(unfound)?;
        // One that the describe doesn't find counts as deleted: EC2 describes an instance for
        // about an hour after it is terminated, and each id a run deletes was found by a describe
        // once already, its create's list or the run's own.
        let (mut ending, mut failures) = (BTreeSet::new(), Vec::new());
        // The instances whose timers are deleted: each terminated, and each that EC2 no longer
        // describes. The timer of one that may still run is kept, to end it all the same.
        let mut ended: BTreeSet<&InstanceId> = (asked.iter())
            .filter(|id| !found.iter().any(|found| found.instance_id == **id))
            .collect();
        for found in &found {
            let id = &found.instance_id;
            if !found.is_of(run) {
                failures.push(format!(
                    "instance `{id}` is no sandbox of run `{}`, so it was left: name only the \
                     run's own, which its list finds",
                    run.as_str()
                ));
                continue;
            }
            match found.state.name {
                State::Terminated => {
                    ended.insert(id);
                }
                State::ShuttingDown => {
                    ending.insert(id);
                }
                // Each on its own, as EC2 terminates none of a call's instances if it refuses
                // one.
                State::Pending | State::Running | State::Stopping | State::Stopped => {
                    match self.terminate(id, Bound::Call) {
                        Ok(()) => {
                            ending.insert(id);
                        }
                        Err(left) => failures.push(left),
                    }
                }
            }
        }
        match self.terminated(ending.clone()) {
            Ok(()) => ended.extend(ending),
            Err(error) => failures.push(error),
        }
        let untimed = ended.into_iter().map(|id| self.unset_timer(run, id));
        failures.extend(untimed.filter_map(Result::err));
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }
}

impl Reached {
    /// Deletes the timer of instance `id`, of run `run`, once the instance is terminated; one
    /// gone already, as one that fired, counts as deleted.
    fn unset_timer(&self, run: &RunTag, id: &InstanceId) -> Result<(), String> {
        let group = &self.group.0;
        let name = timer_name(run, id);
        let args = [
            "scheduler",
            "delete-schedule",
            "--group-name",
            group,
            "--name",
            &name,
        ];
        (self.cli.run_found(&args, Bound::Call).map(drop)).context(format_args!(
            "deleting terminated instance `{id}`'s timer, schedule `{group}/{name}`, failed"
        ))
    }

    /// Describes instances `ids`, every [`DESCRIBE_EVERY`], until EC2 describes each as
    /// terminated, or no more, for up to [`TERMINATED_TIMEOUT`].
    fn terminated(&self, mut ids: BTreeSet<&InstanceId>) -> Result<(), String> {
        let by = Deadline::after(TERMINATED_TIMEOUT);
        while !ids.is_empty() {
            let waiting = format_args!("waiting for {} to be terminated failed", shown(&ids));
            let found = (self.described(&[ids_filter(ids.iter().copied())], Bound::Call))
                .context(waiting)?;
            let unterminated: BTreeSet<&InstanceId> = (found.iter())
                .filter(|found| !matches!(found.state.name, State::Terminated))
                .map(|found| &found.instance_id)
                .collect();
            // One that EC2 no longer describes counts as terminated, as one a delete never found
            // counts as deleted.
            ids.retain(|id| unterminated.contains(id));
            if ids.is_empty() {
                break;
            }
            let Some(left) = by.remaining() else {
                return Err(format!(
                    "EC2 didn't describe {} as terminated within {} s of the delete's \
                     terminations: expected it to end each by then, and each may still run",
                    shown(&ids),
                    TERMINATED_TIMEOUT.as_secs()
                ));
            };
            thread::sleep(left.min(DESCRIBE_EVERY));
        }
        Ok(())
    }

    /// The instances that `filters` find, each a filter that `aws ec2 describe-instances
    /// --filters` takes, with `aws` killed at `bound`.
    fn described(&self, filters: &[String], bound: Bound) -> Result<Vec<ListedInstance>, String> {
        let mut args = vec!["ec2", "describe-instances", "--filters"];
        args.extend(filters.iter().map(String::as_str));
        let Reservations { reservations } = self.cli.json(&args, bound)?;
        let instances = reservations.into_iter().flat_map(|found| found.instances);
        Ok(instances.collect())
    }

    /// Begins to terminate instance `id`, with `aws` killed at `bound`, after which the instance
    /// never runs its sandbox again; or fails, saying that the instance is left.
    fn terminate(&self, id: &InstanceId, bound: Bound) -> Result<(), String> {
        let terminate = ["ec2", "terminate-instances", "--instance-ids", &id.0];
        (self.cli.run(&terminate, bound).map(drop)).context(format_args!(
            "instance `{id}` is left, as terminating it failed"
        ))
    }
}

/// The name of the timer of instance `id`, of run `run`: `<run>.<id>`, which only that
/// instance's timer has, as EC2 never reuses an ID.
fn timer_name(run: &RunTag, id: &InstanceId) -> String {
    format!("{}.{id}", run.as_str())
}

/// A run's hard end ([`Create::hard_end`]) in whole seconds since the Unix epoch, rounded down:
/// when an instance's boot has it powered off by.
#[derive(Clone, Copy)]
struct HardEnd(u64);

impl HardEnd {
    fn of(hard_end: UnixTime) -> Self {
        Self(hard_end.since_epoch().as_secs())
    }

    /// When the instance's timer is set for, in seconds since the Unix epoch: the hard end rounded
    /// up to the minute.
    // `EventBridge` Scheduler invokes a target within the minute its time is in, so a time on the
    // minute after the hard end fires only past it, after the boot's own timer, which a healthy
    // instance ends by first.
    fn timer(self) -> u64 {
        self.0.div_ceil(60) * 60
    }
}

/// The filter of `aws ec2 describe-instances` that finds run `run`'s instances, by their
/// [`RUN_TAG`].
fn run_filter(run: &RunTag) -> String {
    format!("Name=tag:{RUN_TAG},Values={}", run.as_str())
}

/// The filter of `aws ec2 describe-instances` that finds the instances `ids` name, at least one;
/// unlike `--instance-ids`, it doesn't fail on an ID of no instance.
fn ids_filter<'a>(ids: impl IntoIterator<Item = &'a InstanceId>) -> String {
    let ids: Vec<&str> = ids.into_iter().map(|id| id.0.as_str()).collect();
    format!("Name=instance-id,Values={}", ids.join(","))
}

/// `ids`, as an error shows them.
fn shown(ids: &BTreeSet<impl fmt::Display>) -> String {
    let shown: Vec<String> = ids.iter().map(|id| format!("`{id}`")).collect();
    shown.join(", ")
}

/// The ID of an EC2 instance: `i-` and 8 or 17 lowercase hexadecimal digits, as EC2 writes it,
/// such as `i-0123456789abcdef0`.
#[derive(Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(try_from = "String")]
struct InstanceId(String);

impl TryFrom<String> for InstanceId {
    type Error = String;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        let hex = |byte: u8| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte);
        let digits = id
            .strip_prefix("i-")
            .filter(|digits| matches!(digits.len(), 8 | 17));
        if !digits.is_some_and(|digits| digits.bytes().all(hex)) {
            return Err(format!(
                "`{}` is not an instance's ID: use one such as `i-0123456789abcdef0`",
                id.escape_debug()
            ));
        }
        Ok(Self(id))
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What `aws ec2 run-instances` prints, of the one instance it launched.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Launched {
    instances: [LaunchedInstance; 1],
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LaunchedInstance {
    instance_id: InstanceId,
    image_id: String,
}

/// What `aws ec2 describe-instances` prints.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Reservations {
    reservations: Vec<Reservation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Reservation {
    instances: Vec<ListedInstance>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListedInstance {
    instance_id: InstanceId,
    state: InstanceState,
    /// Its tags, which EC2 leaves out of the description of an instance that has none.
    #[serde(default)]
    tags: Vec<Tag>,
}

impl ListedInstance {
    /// Whether the instance is run `run`'s: its [`RUN_TAG`] holds the run's tag.
    fn is_of(&self, run: &RunTag) -> bool {
        (self.tags.iter()).any(|Tag { key, value }| key == RUN_TAG && value == run.as_str())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Tag {
    key: String,
    value: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InstanceState {
    name: State,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum State {
    Pending,
    Running,
    ShuttingDown,
    Terminated,
    Stopping,
    Stopped,
}

/// What `aws ec2 get-console-output` prints: the console's output, which EC2 sends in base64
/// and `aws` decodes, or none, if EC2 has none.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ConsoleOutput {
    output: Option<String>,
}

/// The user data that boots a sandbox's instance, in region `region`: [`ENTRY`], filled in from
/// `start`, which it hands `ec2-boot`.
fn user_data(start: &Start, region: &str) -> String {
    let Ok(json) = serde_json::to_string(start) else {
        unreachable!(
            "a start can't be written as JSON, though it holds only strings, numbers, booleans \
             and an address"
        )
    };
    let filled = [
        ("@PREFIX@", CONSOLE_PREFIX.to_owned()),
        ("@HARD_END@", start.hard_end_unix_seconds.to_string()),
        ("@PULLS_BY@", start.pulls_by_unix_seconds.to_string()),
        ("@REGION@", quote(region)),
        ("@REGISTRY@", quote(&start.registry)),
        ("@SANDBOXD_IMAGE@", quote(&start.sandboxd_image)),
        ("@BOOT_IN_IMAGE@", BOOT_IN_IMAGE.to_owned()),
        ("@GRANT_DIR@", GRANT_DIR.to_owned()),
        // On one line, which is never the heredoc's delimiter, as JSON begins with `{`.
        ("@START@", json),
    ];
    (filled.iter()).fold(ENTRY.to_owned(), |script, (placeholder, value)| {
        script.replace(placeholder, value)
    })
}

/// The tags of the instance that a create of sandbox `name`, for run `run`'s role `role`,
/// launches, and of its volume, as `aws ec2 run-instances --tag-specifications` takes them.
fn tag_specifications(run: &RunTag, name: &Name, role: &Name) -> String {
    // `<run>.<name>`, what EC2's console shows of the instance: which sandbox of which run it
    // is, read back as both, as a name holds no `.`.
    let named = format!("{}.{name}", run.as_str());
    let tags = [
        (RUN_TAG, run.as_str()),
        (ROLE_TAG, role.as_str()),
        ("Name", &named),
    ];
    let tags: Vec<Value> = (tags.iter())
        .map(|(key, value)| json!({ "Key": key, "Value": value }))
        .collect();
    let specifications = json!([
        { "ResourceType": "instance", "Tags": tags },
        { "ResourceType": "volume", "Tags": tags },
    ]);
    specifications.to_string()
}

/// Writes `text` to a new file at `path`, in `dir`, which it makes if it is missing, that only
/// this user can read.
fn write_private(dir: &Path, path: &Path, text: &str) -> Result<(), String> {
    let opened = fs::create_dir_all(dir)
        .and_then(|()| (OpenOptions::new().write(true).create_new(true).mode(0o600)).open(path));
    let written = opened.and_then(|mut file| file.write_all(text.as_bytes()));
    written.map_err(|error| {
        format!(
            "the user data's file {} can't be written: {error}: expected its directory to take a \
             new file of this user's",
            path.display()
        )
    })
}

/// `word`, as one word of a shell's command line: as it is, if every byte is one the shell
/// takes literally, or else in single quotes.
fn quote(word: &str) -> String {
    let literal = |byte: u8| byte.is_ascii_alphanumeric() || b"-_./:=,@+%".contains(&byte);
    if !word.is_empty() && word.bytes().all(literal) {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}
