//! `pumpkin-admin policy`: each principal's IAM policy, as the calls of this build's code on AWS
//! state it, which a deployment's review reads.

use expect_test::{Expect, expect};

use super::Scratch;

/// Compares what `pumpkin-admin policy <arg>` prints with `expect`; it says nothing else,
/// and succeeds.
fn check(arg: &str, expect: &Expect) {
    let scratch = Scratch::new();
    let output = (scratch.admin(&["policy", arg])).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    expect.assert_eq(&String::from_utf8(output.stdout).unwrap());
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn controller() {
    check(
        "controller",
        &expect![[r#"
        {
          "Statement": [
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
                "ecr:BatchCheckLayerAvailability",
                "ecr:BatchGetImage",
                "ecr:CompleteLayerUpload",
                "ecr:DescribeRepositories",
                "ecr:InitiateLayerUpload",
                "ecr:PutImage",
                "ecr:UploadLayerPart"
              ],
              "Effect": "Allow",
              "Resource": [
                "${modal_repository_arn}"
              ]
            },
            {
              "Action": [
                "ecr:BatchCheckLayerAvailability",
                "ecr:BatchGetImage",
                "ecr:CompleteLayerUpload",
                "ecr:DescribeRepositories",
                "ecr:InitiateLayerUpload",
                "ecr:PutImage",
                "ecr:UploadLayerPart"
              ],
              "Effect": "Allow",
              "Resource": [
                "${roles_repository_arn}",
                "${sandboxd_repository_arn}"
              ]
            },
            {
              "Action": [
                "iam:PassRole"
              ],
              "Condition": {
                "StringEquals": {
                  "iam:PassedToService": [
                    "ec2.amazonaws.com"
                  ]
                }
              },
              "Effect": "Allow",
              "Resource": [
                "${sandbox_role_arn}"
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
                "iam:PassRole"
              ],
              "Condition": {
                "StringEquals": {
                  "iam:PassedToService": [
                    "scheduler.amazonaws.com"
                  ]
                }
              },
              "Effect": "Allow",
              "Resource": [
                "${timer_role_arn}"
              ]
            },
            {
              "Action": [
                "ec2:RunInstances"
              ],
              "Condition": {
                "ArnEquals": {
                  "ec2:LaunchTemplate": ${jsonencode(launch_template_arns)}
                },
                "Bool": {
                  "ec2:IsLaunchTemplateResource": [
                    "true"
                  ]
                }
              },
              "Effect": "Allow",
              "Resource": [
                "*"
              ]
            },
            {
              "Action": [
                "ec2:DescribeInstances",
                "ecr:GetAuthorizationToken"
              ],
              "Effect": "Allow",
              "Resource": [
                "*"
              ]
            },
            {
              "Action": [
                "ec2:GetConsoleOutput",
                "ec2:TerminateInstances"
              ],
              "Condition": {
                "ArnEquals": {
                  "ec2:InstanceProfile": [
                    "${sandbox_profile_arn}"
                  ]
                },
                "Null": {
                  "ec2:ResourceTag/pumpkin:run": [
                    "false"
                  ]
                }
              },
              "Effect": "Allow",
              "Resource": [
                "arn:${partition}:ec2:${region}:${account}:instance/*"
              ]
            },
            {
              "Action": [
                "ec2:CreateTags"
              ],
              "Condition": {
                "StringEquals": {
                  "ec2:CreateAction": [
                    "RunInstances"
                  ]
                }
              },
              "Effect": "Allow",
              "Resource": [
                "arn:${partition}:ec2:${region}:${account}:instance/*",
                "arn:${partition}:ec2:${region}:${account}:volume/*"
              ]
            },
            {
              "Action": [
                "scheduler:CreateSchedule",
                "scheduler:DeleteSchedule"
              ],
              "Effect": "Allow",
              "Resource": [
                "arn:${partition}:scheduler:${region}:${account}:schedule/${timer_group}/*"
              ]
            }
          ],
          "Version": "2012-10-17"
        }
    "#]],
    );
}

#[test]
fn timer() {
    check(
        "timer",
        &expect![[r#"
        {
          "Statement": [
            {
              "Action": [
                "ec2:TerminateInstances"
              ],
              "Condition": {
                "ArnEquals": {
                  "ec2:InstanceProfile": [
                    "${sandbox_profile_arn}"
                  ]
                },
                "Null": {
                  "ec2:ResourceTag/pumpkin:run": [
                    "false"
                  ]
                }
              },
              "Effect": "Allow",
              "Resource": [
                "arn:${partition}:ec2:${region}:${account}:instance/*"
              ]
            }
          ],
          "Version": "2012-10-17"
        }
    "#]],
    );
}

#[test]
fn sandbox() {
    check(
        "sandbox",
        &expect![[r#"
        {
          "Statement": [
            {
              "Action": [
                "ecr:BatchGetImage",
                "ecr:GetDownloadUrlForLayer"
              ],
              "Effect": "Allow",
              "Resource": [
                "${roles_repository_arn}",
                "${sandboxd_repository_arn}"
              ]
            },
            {
              "Action": [
                "ecr:GetAuthorizationToken"
              ],
              "Effect": "Allow",
              "Resource": [
                "*"
              ]
            }
          ],
          "Version": "2012-10-17"
        }
    "#]],
    );
}

#[test]
fn modal_pull() {
    check(
        "modal-pull",
        &expect![[r#"
        {
          "Statement": [
            {
              "Action": [
                "ecr:BatchCheckLayerAvailability",
                "ecr:BatchGetImage",
                "ecr:GetDownloadUrlForLayer"
              ],
              "Effect": "Allow",
              "Resource": [
                "${modal_repository_arn}"
              ]
            },
            {
              "Action": [
                "ecr:GetAuthorizationToken"
              ],
              "Effect": "Allow",
              "Resource": [
                "*"
              ]
            }
          ],
          "Version": "2012-10-17"
        }
    "#]],
    );
}

#[test]
fn client() {
    check(
        "client",
        &expect![[r#"
        {
          "Statement": [
            {
              "Action": [
                "ec2:StartInstances"
              ],
              "Effect": "Allow",
              "Resource": [
                "${controller_instance_arn}"
              ]
            },
            {
              "Action": [
                "ec2:DescribeInstances"
              ],
              "Effect": "Allow",
              "Resource": [
                "*"
              ]
            }
          ],
          "Version": "2012-10-17"
        }
    "#]],
    );
}
