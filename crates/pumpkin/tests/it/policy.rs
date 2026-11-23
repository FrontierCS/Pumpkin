//! `pumpkin-admin policy` prints each principal's IAM policy, as the calls of this build's code on
//! AWS state it. A deployment's review reads it. `deploy/aws` renders each policy from
//! `deploy/aws/policies/<principal>.json.tftpl`, which these tests keep equal to it.

use expect_test::{ExpectFile, expect, expect_file};

use super::Scratch;

/// What `pumpkin-admin policy <arg>` prints. It says nothing else, and succeeds.
fn output(arg: &str) -> String {
    let scratch = Scratch::new();
    let output = (scratch.admin(&["policy", arg])).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// Compares `principal`'s policy with `expect`, the module's file of it.
fn check(principal: &str, expect: &ExpectFile) {
    expect.assert_eq(&output(principal));
}

#[test]
fn controller() {
    check(
        "controller",
        &expect_file!["../../../../deploy/aws/policies/controller.json.tftpl"],
    );
}

#[test]
fn timer() {
    check(
        "timer",
        &expect_file!["../../../../deploy/aws/policies/timer.json.tftpl"],
    );
}

#[test]
fn sandbox() {
    let policy = output("sandbox");
    // An image's tag is its digest, and ECR serves `tags/list` as `ecr:ListImages`: with no list,
    // an instance can't name another role's image, such as a judge's.
    assert!(
        !policy.contains("ecr:List") && !policy.contains("ecr:Describe"),
        "{policy}"
    );
    expect_file!["../../../../deploy/aws/policies/sandbox.json.tftpl"].assert_eq(&policy);
}

#[test]
fn modal_pull() {
    check(
        "modal-pull",
        &expect_file!["../../../../deploy/aws/policies/modal-pull.json.tftpl"],
    );
}

#[test]
fn client() {
    check(
        "client",
        &expect_file!["../../../../deploy/aws/policies/client.json.tftpl"],
    );
}

#[test]
fn contributor() {
    check(
        "contributor",
        &expect_file!["../../../../deploy/aws/policies/contributor.json.tftpl"],
    );
}

#[test]
fn check_principal() {
    expect![[r#"
        {
          "Statement": [
            {
              "Action": [
                "iam:GetInstanceProfile"
              ],
              "Effect": "Allow",
              "Resource": [
                "${controller_profile_arn}"
              ]
            },
            {
              "Action": [
                "iam:SimulatePrincipalPolicy"
              ],
              "Effect": "Allow",
              "Resource": [
                "${controller_role_arn}",
                "${modal_pull_role_arn}",
                "${sandbox_role_arn}",
                "${timer_role_arn}"
              ]
            },
            {
              "Action": [
                "ssm:GetParameter"
              ],
              "Effect": "Allow",
              "Resource": [
                "${members_parameter_arn}"
              ]
            },
            {
              "Action": [
                "iam:GetInstanceProfile"
              ],
              "Effect": "Allow",
              "Resource": [
                "${sandbox_profile_arn}"
              ]
            },
            {
              "Action": [
                "ssm:GetParameter"
              ],
              "Effect": "Allow",
              "Resource": [
                "${settings_parameter_arn}"
              ]
            },
            {
              "Action": [
                "ec2:DescribeInstances",
                "ec2:DescribeLaunchTemplateVersions"
              ],
              "Effect": "Allow",
              "Resource": [
                "*"
              ]
            }
          ],
          "Version": "2012-10-17"
        }
    "#]]
    .assert_eq(&output("check"));
}
