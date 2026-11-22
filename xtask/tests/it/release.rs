//! Whether a release may publish, given its commit's statuses on GitHub.

use expect_test::expect;

use super::gate;

// Each case: the commit's statuses, newest first, as GitHub lists them, then whether it passes.
#[test]
fn passed() {
    let status = |context: &str, state: &str| {
        format!(r#"{{"context":"{context}","state":"{state}","description":"{state} at once"}}"#)
    };
    let (success, failure) = (
        status("pumpkin/examples", "success"),
        status("pumpkin/examples", "failure"),
    );
    let other = status("ci", "success");
    let cases = [
        ("none", "[]".to_owned()),
        ("only another context", format!("[{other}]")),
        ("a success", format!("[{other},{success}]")),
        (
            "a failure after a success",
            format!("[{failure},{success}]"),
        ),
        (
            "a success after a failure",
            format!("[{success},{failure}]"),
        ),
        (
            "pending",
            format!("[{}]", status("pumpkin/examples", "pending")),
        ),
        ("an error object", r#"{"message":"Not Found"}"#.to_owned()),
    ];
    let shown: Vec<String> = (cases.iter())
        .map(|(case, statuses)| {
            let passed = gate::passed(statuses, "51ab6c4");
            format!("{case}: {}", passed.err().as_deref().unwrap_or("passes"))
        })
        .collect();
    expect![[r#"
        none: 51ab6c4 has no `pumpkin/examples` status, so its examples haven't passed there: run `cargo xtask live examples --status` on 51ab6c4
        only another context: 51ab6c4 has no `pumpkin/examples` status, so its examples haven't passed there: run `cargo xtask live examples --status` on 51ab6c4
        a success: passes
        a failure after a success: the latest `pumpkin/examples` status of 51ab6c4 is failure (failure at once): run `cargo xtask live examples --status` on 51ab6c4 again
        a success after a failure: passes
        pending: the latest `pumpkin/examples` status of 51ab6c4 is pending (pending at once): run `cargo xtask live examples --status` on 51ab6c4 again
        an error object: GitHub's statuses of 51ab6c4 aren't a JSON list (invalid type: map, expected a sequence at line 1 column 0): expected one"#]].assert_eq(&shown.join("\n"));
}
