use expect_test::{Expect, expect};
use retention::boot::Unit;

fn check(cases: &[(&str, Unit<'_>)], expect: &Expect) {
    let lines: Vec<String> = (cases.iter())
        .map(|&(name, unit)| format!("{name}: {}", if unit.ended() { "ended" } else { "not yet" }))
        .collect();
    expect.assert_eq(&lines.join("\n"));
}

#[test]
fn prunes_wait_for_the_boot() {
    let at = Some("Sun 2026-11-22 07:00:00 UTC");
    let unit = |active_state, job, exit_timestamp| Unit {
        active_state,
        job,
        exit_timestamp,
    };
    check(
        &[
            ("exited, idle", unit(Some("inactive"), Some(""), at)),
            ("exited, failed", unit(Some("failed"), Some(""), at)),
            (
                "exited, active as a oneshot that remains",
                unit(Some("active"), Some(""), at),
            ),
            (
                "running its first time",
                unit(Some("activating"), Some("123"), Some("")),
            ),
            (
                "activating again after an exit",
                unit(Some("activating"), Some(""), at),
            ),
            (
                "exited, a job queued",
                unit(Some("inactive"), Some("124"), at),
            ),
            (
                "not run this boot",
                unit(Some("inactive"), Some(""), Some("")),
            ),
            ("exited, but no job said", unit(Some("inactive"), None, at)),
            ("systemctl said nothing", unit(None, None, None)),
        ],
        &expect![[r#"
            exited, idle: ended
            exited, failed: ended
            exited, active as a oneshot that remains: ended
            running its first time: not yet
            activating again after an exit: not yet
            exited, a job queued: not yet
            not run this boot: not yet
            exited, but no job said: not yet
            systemctl said nothing: not yet"#]],
    );
}
