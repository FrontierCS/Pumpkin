use expect_test::{Expect, expect};
use provider::RunTag;

fn check(tags: &[&str], expect: &Expect) {
    let outcomes: Vec<String> = (tags.iter())
        .map(|&tag| match RunTag::try_from(tag.to_owned()) {
            Ok(tag) => format!("`{}` parsed", tag.as_str()),
            Err(error) => error,
        })
        .collect();
    expect.assert_eq(&outcomes.join("\n"));
}

#[test]
fn run_tags() {
    check(
        &[
            "20261105-122057-9802092b",
            "pk-4242-cleanup-other",
            "selftest.v2_a",
            "",
            // `docker` trims a filter's value, so this tag would find another run's sandboxes.
            "pk-run ",
            "pk/run",
            "-pk",
            ".pk",
        ],
        &expect![[r#"
            `20261105-122057-9802092b` parsed
            `pk-4242-cleanup-other` parsed
            `selftest.v2_a` parsed
            the run tag is empty: use only ASCII letters, digits, `_`, `.` and `-`, at least one
            the run tag `pk-run ` holds ` `: use only ASCII letters, digits, `_`, `.` and `-`
            the run tag `pk/run` holds `/`: use only ASCII letters, digits, `_`, `.` and `-`
            the run tag `-pk` starts with `-`: start it with an ASCII letter or digit
            the run tag `.pk` starts with `.`: start it with an ASCII letter or digit"#]],
    );
}
