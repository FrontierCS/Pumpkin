//! The check of the EC2 timers of a lost run. Each instance is to have one, a schedule that
//! terminates it at the hard end that the run's log records for its sandbox.
//!
//! It runs once the run's job is killed. Its log is final then, and nothing deletes a timer before
//! the reap, or before the timer fires at the hard end. EC2 lists a terminated instance for a
//! while, with its tags.

use std::time::Duration;

use jiff::Timestamp;
use serde_json::Value;
use stdx::Context as _;

use super::{aws, instances, listed, listed_schedules};
use crate::Result;
use crate::settings::{Provider, Settings};

/// What is wrong with the EC2 instances of run `id` on `provider`, and with their timers, given
/// `log`, the run's log. Each instance is to be a sandbox whose create the log records once, and
/// to have one timer, as [`unfit`] wants it. Each timer is to terminate one of the instances. On
/// EC2, the run is to have an instance.
pub(super) fn mistimed(
    settings: &Settings,
    provider: Provider,
    id: &str,
    log: &str,
) -> Result<Vec<String>> {
    let (unix_ns, created) = recorded(log)?;
    let instances = instances(settings, id)?;
    let timers = timers(settings, id)?;
    let mut wrong = Vec::new();
    if instances.is_empty() && provider == Provider::Ec2 {
        wrong.push(format!(
            "EC2 lists no instance of run {id}: expected one for each of its sandboxes"
        ));
    }
    let mut ids = Vec::new();
    for instance in &instances {
        let Some(instance_id) = instance["InstanceId"].as_str() else {
            wrong.push(format!(
                "EC2 lists an instance of run {id} without its ID: {instance}"
            ));
            continue;
        };
        ids.push(instance_id);
        let held = (instance_id, sandbox_of(instance, id));
        wrong.extend(untimed(held, &created, &timers, unix_ns));
    }
    for timer in &timers {
        let target = terminates(timer);
        if !target
            .as_deref()
            .is_some_and(|target| ids.contains(&target))
        {
            let target = target.unwrap_or_else(|| "no one instance".to_owned());
            wrong.push(format!(
                "{} terminates {target}, none of the instances of run {id} that EC2 lists: \
                 expected each timer of the run to terminate one",
                shown(timer)
            ));
        }
    }
    Ok(wrong)
}

/// What is wrong with the instance `instance_id`, that of `sandbox` if its tags name one. The run's
/// log records each create in `created`, with its sandbox's hard end. The run has `timers`.
fn untimed(
    (instance_id, sandbox): (&str, Option<&str>),
    created: &[(String, u64)],
    timers: &[Value],
    unix_ns: u64,
) -> Option<String> {
    let Some(sandbox) = sandbox else {
        return Some(format!(
            "instance {instance_id} has no tag `Name` of `<run>.<sandbox>`: expected the one that \
             the EC2 provider gives each instance"
        ));
    };
    let hard_ends: Vec<u64> = (created.iter())
        .filter(|(name, _)| name == sandbox)
        .map(|(_, hard_end_ns)| *hard_end_ns)
        .collect();
    let [hard_end_ns] = hard_ends[..] else {
        return Some(format!(
            "sandbox {sandbox}'s instance {instance_id} has {} creates in the run's log: expected \
             one",
            hard_ends.len()
        ));
    };
    let hard_end = format!(
        "its hard end in the run's log, {}.{:09} s after the run's start",
        hard_end_ns / 1_000_000_000,
        hard_end_ns % 1_000_000_000
    );
    let Some(expected) = timer_at(unix_ns, hard_end_ns) else {
        return Some(format!(
            "sandbox {sandbox}'s instance {instance_id} has {hard_end}, for which no timer can \
             be set: expected a time that EventBridge Scheduler can hold"
        ));
    };
    let ours: Vec<&Value> = (timers.iter())
        .filter(|timer| terminates(timer).as_deref() == Some(instance_id))
        .collect();
    let [timer] = ours[..] else {
        return Some(format!(
            "sandbox {sandbox}'s instance {instance_id} has {} timers: expected one, at \
             {expected} in UTC, from {hard_end}",
            ours.len()
        ));
    };
    if let Some(unfit) = unfit(timer, &expected) {
        return Some(format!(
            "sandbox {sandbox}'s instance {instance_id} has its timer, {}, {unfit}: expected \
             {expected} in UTC, from {hard_end}, in whole seconds rounded up to the minute",
            shown(timer)
        ));
    }
    println!(
        "xtask: sandbox {sandbox}'s instance {instance_id} has its timer, {}, at {expected} in \
         UTC, from {hard_end}",
        shown(timer)
    );
    None
}

/// What keeps `timer`, a schedule as `aws scheduler get-schedule` prints it, from terminating its
/// instance at `expected`, an `at` expression in UTC, if anything. It is to be enabled, at
/// `expected`, with no flexible window, and to call EC2's `TerminateInstances`, as the EC2 provider
/// sets it.
fn unfit(timer: &Value, expected: &str) -> Option<String> {
    let (state, window) = (&timer["State"], &timer["FlexibleTimeWindow"]["Mode"]);
    let (at, zone) = (
        &timer["ScheduleExpression"],
        &timer["ScheduleExpressionTimezone"],
    );
    let target = &timer["Target"]["Arn"];
    // The universal target that terminates instances, in any partition:
    // https://docs.aws.amazon.com/scheduler/latest/UserGuide/managing-targets-universal.html
    let terminating = (target.as_str())
        .and_then(|arn| arn.strip_prefix("arn:")?.split_once(':'))
        .is_some_and(|(_, rest)| rest == "scheduler:::aws-sdk:ec2:terminateInstances");
    if *state != "ENABLED" {
        Some(format!("whose state is {state}"))
    } else if *window != "OFF" {
        Some(format!("whose flexible time window is {window}"))
    } else if !terminating {
        Some(format!("whose target is {target}"))
    } else if at.as_str() != Some(expected) || *zone != "UTC" {
        Some(format!("at {at} in {zone}"))
    } else {
        None
    }
}

/// The sandbox whose instance `instance` is, from its tag `Name`, `<run>.<sandbox>`, as
/// `aws ec2 describe-instances` prints it for run `id`.
fn sandbox_of<'i>(instance: &'i Value, id: &str) -> Option<&'i str> {
    let mut tags = instance["Tags"].as_array().into_iter().flatten();
    let name = tags.find(|tag| tag["Key"] == "Name")?["Value"].as_str()?;
    name.strip_prefix(id)?.strip_prefix('.')
}

/// Each schedule of run `id`, as `aws scheduler get-schedule` prints it.
fn timers(settings: &Settings, id: &str) -> Result<Vec<Value>> {
    let grant = "grant these credentials `scheduler:GetSchedule` on the timer's schedules, as the \
                 policy that deploy/aws outputs as `contributor_policy_arn` does, and run this again";
    let mut timers = Vec::new();
    for schedule in listed_schedules(settings, id, None)? {
        let (Some(group), Some(name)) = (schedule["GroupName"].as_str(), schedule["Name"].as_str())
        else {
            return Err(
                format!("a schedule that EventBridge listed has no name: {schedule}").into(),
            );
        };
        let get = [
            "scheduler",
            "get-schedule",
            "--group-name",
            group,
            "--name",
            name,
        ];
        timers.push(listed(&mut aws(settings, &get), Some(grant))?);
    }
    Ok(timers)
}

/// `timer`, a schedule as `aws scheduler get-schedule` prints it, as `schedule <group>/<name>`.
fn shown(timer: &Value) -> String {
    format!("schedule {}/{}", timer["GroupName"], timer["Name"])
}

/// The `unix_ns` of the `run_started` that begins `log`, a run's log, and each sandbox's name and
/// `hard_end_ns` in its `sandbox_created`. A line counts once its newline is there.
fn recorded(log: &str) -> Result<(u64, Vec<(String, u64)>)> {
    let ns = |value: &Value| value.as_str().and_then(|ns| ns.parse::<u64>().ok());
    let read = |line: &str| -> Result<Value> {
        Ok(serde_json::from_str(line).context("a line of the run's log isn't JSON")?)
    };
    let mut lines = log
        .split_inclusive('\n')
        .filter(|line| line.ends_with('\n'));
    let first = read(lines.next().unwrap_or_default())?;
    let started = &first["event"];
    let Some(unix_ns) = ns(&started["unix_ns"]).filter(|_| started["kind"] == "run_started") else {
        return Err(format!(
            "the run's log begins with {first}: expected `run_started`, with its `unix_ns`"
        )
        .into());
    };
    let mut created = Vec::new();
    for line in lines {
        let line = read(line)?;
        let event = &line["event"];
        if event["kind"] != "sandbox_created" {
            continue;
        }
        let (Some(sandbox), Some(hard_end_ns)) =
            (event["sandbox"].as_str(), ns(&event["hard_end_ns"]))
        else {
            return Err(format!(
                "the run's log holds {line}: expected each `sandbox_created` to name its \
                 sandbox, with its `hard_end_ns`"
            )
            .into());
        };
        created.push((sandbox.to_owned(), hard_end_ns));
    }
    Ok((unix_ns, created))
}

/// The instance that `timer`, a schedule as `aws scheduler get-schedule` prints it, terminates:
/// the one ID in its target's input, as the EC2 provider sets it.
fn terminates(timer: &Value) -> Option<String> {
    let input: Value = serde_json::from_str(timer["Target"]["Input"].as_str()?).ok()?;
    match input["InstanceIds"].as_array()?.as_slice() {
        [id] => id.as_str().map(str::to_owned),
        _ => None,
    }
}

/// The `at` expression in UTC of an instance's timer. The instance's hard end is `hard_end_ns`
/// after `unix_ns`. The timer is at the hard end in whole seconds, rounded down, then rounded up
/// to the minute. None if that is past what a `Timestamp` holds.
// As `HardEnd::of` and `HardEnd::timer` in `providers/ec2/src/launch.rs` round it, and as
// `set_timer` there writes it. Calling them would pass whatever they do.
fn timer_at(unix_ns: u64, hard_end_ns: u64) -> Option<String> {
    let seconds = unix_ns.checked_add(hard_end_ns)? / 1_000_000_000;
    let timer = Duration::from_secs(seconds.div_ceil(60) * 60);
    let timer = Timestamp::UNIX_EPOCH.checked_add(timer).ok()?;
    Some(timer.strftime("at(%Y-%m-%dT%H:%M:%S)").to_string())
}
