//! What `pumpkin-admin check` decides of the controller's instance from what EC2 and IAM describe
//! of it (`instance`, which this binary compiles itself, as the crate keeps it private): each
//! case breaks one part of the description of the instance as deploy/aws makes it, and the
//! snapshot shows each problem, and what goes unchecked.

use expect_test::{Expect, expect};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

#[path = "../../src/deployment/instance.rs"]
#[expect(dead_code, reason = "these tests leave out what the other checks read")]
mod instance;

use instance::Described;

/// What EC2 and IAM describe of the controller's instance as deploy/aws makes it, running: the
/// instance, and its instance profile, read as a `Result` in serde's form, or `null` for none
/// read.
fn healthy() -> Value {
    let profile = "arn:aws:iam::123456789012:instance-profile/pumpkin-controller";
    json!({
        "instance": {
            "InstanceId": "i-0123456789abcdef0",
            "State": { "Code": 16, "Name": "running" },
            "PublicIpAddress": "203.0.113.7",
            "IamInstanceProfile": { "Arn": profile },
        },
        "profile": { "Ok": {
            "Arn": profile,
            "Roles": [{
                "Arn": "arn:aws:iam::123456789012:role/pumpkin-controller",
            }],
        } },
    })
}

fn part<T: DeserializeOwned>(value: &Value) -> T {
    serde_json::from_value(value.clone()).unwrap()
}

/// A case: what it stands for, and its edit of the healthy description.
type Case = (&'static str, fn(&mut Value));

/// Checks each case, the healthy description with an edit, against `expect`: the problems that
/// the check reports of the instance, then what it leaves unchecked.
fn check(cases: &[Case], expect: &Expect) {
    let lines = cases.iter().map(|(case, edit)| {
        let mut description = healthy();
        edit(&mut description);
        let described = Described {
            instance: part(&description["instance"]),
            profile: part(&description["profile"]),
        };
        let problems = described.problems("controller").into_iter();
        let unchecked = described.unchecked("controller").into_iter();
        let found = (problems.map(|wrong| format!("\n  {wrong}")))
            .chain(unchecked.map(|wrong| format!("\n  unchecked: {wrong}")));
        format!("{case}:{}", found.collect::<String>())
    });
    expect.assert_eq(&lines.collect::<Vec<_>>().join("\n"));
}

#[test]
fn controller_instance() {
    check(
        &[
            ("as deploy/aws makes it", |_| {}),
            ("no instance profile", |it| {
                it["instance"]["IamInstanceProfile"] = Value::Null;
                it["profile"] = Value::Null;
            }),
            ("its profile's read failing", |it| {
                it["profile"] = json!({ "Err": "denied" });
            }),
            ("a profile without a role", |it| {
                it["profile"]["Ok"]["Roles"] = json!([]);
            }),
            ("running without a public address", |it| {
                it["instance"]["PublicIpAddress"] = Value::Null;
            }),
            ("stopped, without a public address", |it| {
                it["instance"]["State"] = json!({ "Code": 80, "Name": "stopped" });
                it["instance"]["PublicIpAddress"] = Value::Null;
            }),
            ("pending", |it| {
                it["instance"]["State"] = json!({ "Code": 0, "Name": "pending" });
            }),
        ],
        &expect![[r#"
            as deploy/aws makes it:
            no instance profile:
              controller: it has no instance profile, whose role its jobs call AWS with: apply deploy/aws, which gives it one
            its profile's read failing:
              controller: reading its instance profile failed: denied: check with credentials that `pumpkin-admin policy check` allows
            a profile without a role:
              controller: its instance profile arn:aws:iam::123456789012:instance-profile/pumpkin-controller holds no role: give it the controller's, as deploy/aws does
            running without a public address:
              controller: it runs with no public IPv4 address, which its jobs reach AWS and Modal through, and which Modal's tunnels admit: give it one, as its launch by deploy/aws does
            stopped, without a public address:
              unchecked: controller: it is stopped, so its public IPv4 address is unchecked: run `pumpkin ls` to start it, then check again
            pending:
              unchecked: controller: it is starting, so its public IPv4 address is unchecked: check again once it runs"#]],
    );
}
