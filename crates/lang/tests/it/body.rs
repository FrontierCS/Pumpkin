//! Bodies, through `lang::check` and `lang::call`, with a host written here: what a body's
//! handle does, what it returns, and what a check of it refuses, which a call refuses alike.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use expect_test::{Expect, expect};
use lang::{Args, Body, BodyKind, Bounds, Host, Label};
use schema::ir::{AbsolutePath, ArgumentValue, Argv, Name};
use serde_json::{Value, json};

use crate::{Case, Sources, files, on_stack};

/// What the host's `get` gives, of the one file it has.
const RESULT: (&str, &str) = (
    "/pumpkin/out/result.json",
    r#"{"raw_score": 43.12, "failure": "slow", "metrics": {"passed": 7, "total": 10}}"#,
);

/// A host that logs each method a handle calls, a line each.
#[derive(Default)]
struct Logged(Vec<String>);

impl lang::Target for Logged {
    fn caller(&self) -> Option<&str> {
        Some("agent")
    }

    fn put(&mut self, path: &AbsolutePath, input: usize) -> Result<(), String> {
        self.0.push(format!("put payload {input} at {path}"));
        Ok(())
    }

    fn run(&mut self, argv: &Argv) -> Result<(), String> {
        self.0
            .push(format!("run {} {:?}", argv.program(), argv.args()));
        Ok(())
    }

    fn get(&mut self, path: &AbsolutePath) -> Result<String, String> {
        self.0.push(format!("get {path}"));
        if path.as_str() == RESULT.0 {
            Ok(RESULT.1.to_owned())
        } else {
            Err(format!(
                "the target has no file at `{path}`: get `{}`",
                RESULT.0
            ))
        }
    }
}

impl lang::Run for Logged {
    fn set_submission(&mut self) {
        self.0.push(String::from("set the submission"));
    }

    fn end_work(&mut self) {
        self.0.push(String::from("end the work"));
    }
}

#[track_caller]
fn check(def: &str, kind: BodyKind, keywords: Value, fixture: &str, expect: &Expect) {
    let given = json!({});
    check_within(
        Bounds::WORKFLOW,
        def,
        kind,
        keywords,
        given,
        fixture,
        expect,
    );
}

fn caller_arguments(given: Value) -> BTreeMap<Name, ArgumentValue> {
    let Value::Object(given) = given else {
        panic!("the arguments are {given}: give an object")
    };
    given
        .into_iter()
        .map(|(name, value)| {
            let value = match value {
                Value::String(text) => ArgumentValue::Str(text),
                Value::Bool(bool) => ArgumentValue::Bool(bool),
                Value::Number(int) => ArgumentValue::Int(int.as_i64().unwrap()),
                other => panic!("an argument can't be {other}"),
            };
            (Name::try_from(name).unwrap(), value)
        })
        .collect()
}

/// Checks, against `expect`, what a call of `def`, a body of `kind`, gives with `keywords`, a JSON
/// object, and the caller's arguments that `given`, an object of strings, ints and bools, holds,
/// within `bounds`: each method its handle calls, then each name and value it returns, in order;
/// or else the problems. `fixture` is a case as `Case::parse` reads it, whose first file is the
/// body's. Asserts that `lang::check` refuses as the call does, or accepts the body.
#[track_caller]
fn check_within(
    bounds: Bounds,
    def: &str,
    kind: BodyKind,
    keywords: Value,
    given: Value,
    fixture: &str,
    expect: &Expect,
) {
    let Value::Object(keywords) = keywords else {
        panic!("the keywords are {keywords}: give an object")
    };
    let arguments = caller_arguments(given);
    let files = files();
    let shown = on_stack(|| {
        let (args, case) = Case::parse(fixture, &files, bounds);
        let args = Args::parse(args).unwrap();
        let file = Label::parse(&format!("//{}", case.entry)).unwrap();
        let body = Body {
            file: &file,
            def,
            keywords: &keywords,
        };
        let mut sources = Sources {
            held: &case.held,
            files: &files,
        };
        let checked = lang::check(&body, kind, &mut sources, &args, bounds);
        let mut host = Logged::default();
        let submissions = [("z", Some(1)), ("a", None), ("b", Some(0))]
            .map(|(role, slot)| (Name::try_from(role.to_owned()).unwrap(), slot));
        let host_of_kind = match kind {
            BodyKind::Job => Host::Job(&mut host),
            BodyKind::Now => Host::Now(&mut host),
            BodyKind::Final => Host::Final(&mut host, &submissions),
        };
        let called = lang::call(&body, host_of_kind, &arguments, &mut sources, &args, bounds);
        match (checked, called) {
            (Ok(()), called) => {
                let logged: String = host.0.iter().flat_map(|line| [line, "\n"]).collect();
                match called {
                    Ok(returned) => {
                        let mut shown = format!("{logged}returns");
                        for (name, value) in &returned {
                            write!(shown, " {name}={value}").unwrap();
                        }
                        shown
                    }
                    Err(problems) => format!("{logged}{problems}"),
                }
            }
            (Err(refused), called) => {
                let called = called.map_err(|problems| problems.to_string());
                assert_eq!(
                    called,
                    Err(refused.to_string()),
                    "the call refuses as the check"
                );
                assert_eq!(
                    host.0,
                    Vec::<String>::new(),
                    "a refused body calls no method"
                );
                refused.to_string()
            }
        }
    });
    expect.assert_eq(&shown);
}

#[test]
fn targets() {
    check(
        "train_call",
        BodyKind::Job,
        json!({"metrics": ["passed", "absent"]}),
        r#"
#- //lib/verify.star
PAYLOAD = "/pumpkin/in/payload"

def _verified(target: Target, payload: Payload):
    target.put(PAYLOAD, payload)
    target.run(["/opt/frontier/verifier", "--fast", target.caller()])
    return json.decode(target.get("/pumpkin/out/result.json"))

def train_call(target: Target, payload: Payload, *, metrics: list[str]):
    result = _verified(target, payload)
    found = result.get("metrics", {})
    top = {key: result[key] for key in ["raw_score", "failure"] if key in result}
    return top | {name: found[name] for name in metrics if name in found}
"#,
        &expect![[r#"
            put payload 0 at /pumpkin/in/payload
            run /opt/frontier/verifier ["--fast", "agent"]
            get /pumpkin/out/result.json
            returns raw_score=43.12 failure="slow" passed=7"#]],
    );
    check(
        "final_call",
        BodyKind::Final,
        json!({}),
        r#"
#- //lib/verify.star
def final_call(target: Target, submissions, *, mode: str = "final"):
    _ = [target.put("/in/" + name, submissions[name]) for name in submissions if submissions[name] != None]
    target.put("/another", submissions["z"])
    target.run(["/opt/frontier/verifier", mode])
    return {"score": json.decode(target.get("/pumpkin/out/result.json"))["raw_score"], "missing": submissions["a"]}
"#,
        &expect![[r#"
            put payload 1 at /in/z
            put payload 0 at /in/b
            put payload 1 at /another
            run /opt/frontier/verifier ["final"]
            get /pumpkin/out/result.json
            returns score=43.12 missing=null"#]],
    );
}

#[test]
fn arguments() {
    let submit = r#"
#- //lib/submit.star
def submit(run: Run, payload: Payload, finalize: bool, note: str = "none"):
    run.set_submission(payload)
    if finalize:
        run.end_work()
    return {"note": note}
"#;
    let check = |given, expect| {
        let (keywords, bounds) = (json!({}), Bounds::WORKFLOW);
        check_within(
            bounds,
            "submit",
            BodyKind::Now,
            keywords,
            given,
            submit,
            expect,
        );
    };
    check(
        json!({"finalize": true, "note": "last"}),
        &expect![[r#"
            set the submission
            end the work
            returns note="last""#]],
    );
    check(
        json!({"note": "last"}),
        &expect![[r#"
        lib/submit.star:1:5: `submit` lacks `finalize`: give it
            1 | def submit(run: Run, payload: Payload, finalize: bool, note: str = "none"):
              |     ^^^^^^"#]],
    );
    check(
        json!({"finalize": false, "mode": 1}),
        &expect![[r#"
        lib/submit.star:1:5: `submit` takes no `mode` after the payload, which its caller gives: give only those it takes
            1 | def submit(run: Run, payload: Payload, finalize: bool, note: str = "none"):
              |     ^^^^^^"#]],
    );
    // A job's body takes its caller's arguments as a now function's does.
    let train = r#"
#- //lib/verify.star
def train(target: Target, payload: Payload, runs: int, *, metric: str):
    target.run(["/opt/verify", str(runs), metric])
    return {"runs": runs}
"#;
    let (keywords, given) = (json!({"metric": "time"}), json!({"runs": 3}));
    check_within(
        Bounds::WORKFLOW,
        "train",
        BodyKind::Job,
        keywords,
        given,
        train,
        &expect![[r#"
            run /opt/verify ["3", "time"]
            returns runs=3"#]],
    );
}

#[test]
fn now() {
    check(
        "submit",
        BodyKind::Now,
        json!({}),
        r#"
#- //lib/submit.star
def submit(run: Run, payload: Payload, finalize: bool = False) -> dict[str, str]:
    run.set_submission(payload)
    if finalize:
        run.end_work()
        return {"state": "accepted"}
    return {"state": "stored"}
"#,
        &expect![[r#"
            set the submission
            returns state="stored""#]],
    );
    check(
        "final",
        BodyKind::Now,
        json!({}),
        r#"
#- //lib/submit.star
def final(run, payload):
    run.set_submission(payload)
    run.end_work()
    return {}
"#,
        &expect![[r#"
            set the submission
            end the work
            returns"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshots"
)]
fn refusals() {
    check(
        "judge",
        BodyKind::Final,
        json!({}),
        r#"
#- //lib/judge.star
def judge(target: Target, submissions: Payload):
    return {}
"#,
        &expect![[r#"
            lib/judge.star:1:27: `judge` takes the submissions by role: leave `submissions` unannotated
                1 | def judge(target: Target, submissions: Payload):
                  |                           ^^^^^^^^^^^"#]],
    );
    check(
        "train_call",
        BodyKind::Job,
        json!({}),
        r#"
#- //lib/missing.star
"#,
        &expect![
            "lib/missing.star: no fixture is at `lib/missing.star`: name a file in the fixtures"
        ],
    );
    check(
        "train_call",
        BodyKind::Job,
        json!({}),
        r#"
#- //lib/verify.star
workflow(name = "verify", roles = [], final = None, deadline_seconds = 1)
def train_call(target, payload):
    pass
"#,
        &expect![[r#"
            lib/verify.star:1:1: `workflow()` is called while `//lib/verify.star` loads: call it in the entry file, which loads `//lib/verify.star`
                1 | workflow(name = "verify", roles = [], final = None, deadline_seconds = 1)
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        "train_cal",
        BodyKind::Job,
        json!({}),
        r#"
#- //lib/verify.star
def train_call(target, payload):
    pass
alias = train_call
"#,
        &expect![
            "lib/verify.star: the file defines no function `train_cal`: name one of `train_call`"
        ],
    );
    check(
        "alias",
        BodyKind::Job,
        json!({}),
        r#"
#- //lib/verify.star
load("//lib/other.star", "train_call")
alias = train_call
#- //lib/other.star
def train_call(target, payload):
    pass
"#,
        &expect![
            "lib/verify.star: the file defines no function, so not `alias`: name the file that defines it"
        ],
    );
    check(
        "train_call",
        BodyKind::Job,
        json!({}),
        r#"
#- //lib/verify.star
def train_call(target):
    pass
"#,
        &expect![[r#"
            lib/verify.star:1:5: `train_call` takes 1 positional parameter, and a job's body takes its `Target` and the payload first: write `def train_call(target: Target, payload: Payload, …)`
                1 | def train_call(target):
                  |     ^^^^^^^^^^"#]],
    );
    check(
        "submit",
        BodyKind::Now,
        json!({}),
        r#"
#- //lib/submit.star
def submit(target: Target, payload: str = "", finalize = False, *, mode: int, extra: str = ""):
    pass
"#,
        &expect![[r#"
            lib/submit.star:1:12: `submit`'s first parameter is a `Target`, and a now function's body takes its `Run`: write `def submit(target: Run, …)`
                1 | def submit(target: Target, payload: str = "", finalize = False, *, mode: int, extra: str = ""):
                  |            ^^^^^^
            lib/submit.star:1:28: `submit`'s second parameter is a string, and a now function's body takes the payload second: annotate `payload` `Payload`
                1 | def submit(target: Target, payload: str = "", finalize = False, *, mode: int, extra: str = ""):
                  |                            ^^^^^^^
            lib/submit.star:1:43: `submit`'s `payload` has a default, and a now function's body is always given its `Run` and the payload: drop the default
                1 | def submit(target: Target, payload: str = "", finalize = False, *, mode: int, extra: str = ""):
                  |                                           ^^
            lib/submit.star:1:47: `submit`'s `finalize` follows the payload, so its caller gives it: annotate it `str`, `int` or `bool`
                1 | def submit(target: Target, payload: str = "", finalize = False, *, mode: int, extra: str = ""):
                  |                                               ^^^^^^^^
            lib/submit.star:1:68: `submit` takes the keyword `mode`, and a now function has no `keywords` to give it: give it a default, or make it a positional parameter after the payload, for its caller to give
                1 | def submit(target: Target, payload: str = "", finalize = False, *, mode: int, extra: str = ""):
                  |                                                                    ^^^^"#]],
    );
    check(
        "train_call",
        BodyKind::Job,
        json!({"metrics": ["a", 1], "metric": [], "deep": u64::MAX, "ratio": 0.5}),
        r#"
#- //lib/verify.star
def train_call(target, payload, *, metrics: list[str], deep: int = 0, ratio: int = 1):
    pass
"#,
        &expect![[r#"
            lib/verify.star:1:5: `18446744073709551615`, outside -2^63 to 2^63 - 1, is not in Pumpkin's Starlark: give smaller ints, since ints are 64-bit
                1 | def train_call(target, payload, *, metrics: list[str], deep: int = 0, ratio: int = 1):
                  |     ^^^^^^^^^^
                it is the grant's keyword `deep`
            lib/verify.star:1:5: the grant's `keywords` gives `metric`, which is no keyword-only parameter of `train_call`: remove it
                1 | def train_call(target, payload, *, metrics: list[str], deep: int = 0, ratio: int = 1):
                  |     ^^^^^^^^^^
            lib/verify.star:1:36: an element of the grant's keyword `metrics` can't be `1`: give a string
                1 | def train_call(target, payload, *, metrics: list[str], deep: int = 0, ratio: int = 1):
                  |                                    ^^^^^^^
                the element, `1`, is made at lib/verify.star:1:5
            lib/verify.star:1:71: the grant's keyword `ratio` can't be `0.5`: `train_call` takes an int
                1 | def train_call(target, payload, *, metrics: list[str], deep: int = 0, ratio: int = 1):
                  |                                                                       ^^^^^"#]],
    );
    check(
        "final_call",
        BodyKind::Final,
        json!({}),
        r#"
#- //lib/final.star
def final_call(target: Run, payload, level: int, mode = "x", *, metric, extra = ""):
    pass
"#,
        &expect![[r#"
            lib/final.star:1:16: `final_call`'s first parameter is a `Run`, and the final call's body takes its `Target`: write `def final_call(target: Target, …)`
                1 | def final_call(target: Run, payload, level: int, mode = "x", *, metric, extra = ""):
                  |                ^^^^^^
            lib/final.star:1:38: `final_call`'s `level` follows the submissions, and the final call has no caller to give it: remove it
                1 | def final_call(target: Run, payload, level: int, mode = "x", *, metric, extra = ""):
                  |                                      ^^^^^
            lib/final.star:1:50: `final_call`'s `mode` follows the submissions, and the final call has no caller to give it: remove it
                1 | def final_call(target: Run, payload, level: int, mode = "x", *, metric, extra = ""):
                  |                                                  ^^^^
            lib/final.star:1:65: `final_call` takes the keyword `metric`, which the final call's `keywords` does not give: give it, or a default
                1 | def final_call(target: Run, payload, level: int, mode = "x", *, metric, extra = ""):
                  |                                                                 ^^^^^^"#]],
    );
    let deep = (0..129).fold(json!(1), |inner, _| json!([inner]));
    check(
        "train_call",
        BodyKind::Job,
        json!({"deep": deep}),
        r#"
#- //lib/verify.star
def train_call(target, payload, *, deep):
    pass
"#,
        &expect![[r#"
            lib/verify.star:1:5: this JSON nests arrays and objects more than 128 deep: nest them at most 128 deep
                1 | def train_call(target, payload, *, deep):
                  |     ^^^^^^^^^^
                it is the grant's keyword `deep`"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshots"
)]
fn failures() {
    check(
        "judge",
        BodyKind::Final,
        json!({}),
        r#"
#- //lib/judge.star
def judge(target: Target, submissions):
    target.put("/in/absent", submissions["a"])
    return {}
"#,
        &expect![[r#"
            lib/judge.star:2:30: `put` can't put `None`: give it the payload
                2 |     target.put("/in/absent", submissions["a"])
                  |                              ^^^^^^^^^^^^^^^^
                the argument, `None`, is made at lib/judge.star:1:5
                in `judge()`, called at lib/judge.star:1:5"#]],
    );
    check(
        "train_call",
        BodyKind::Job,
        json!({}),
        r#"
#- //lib/verify.star
def train_call(target, payload):
    target.put("/pumpkin/in/payload", payload)
    return json.decode(target.get("/pumpkin/out/missing.json"))
"#,
        &expect![[r#"
            put payload 0 at /pumpkin/in/payload
            get /pumpkin/out/missing.json
            lib/verify.star:3:24: the target has no file at `/pumpkin/out/missing.json`: get `/pumpkin/out/result.json`
                3 |     return json.decode(target.get("/pumpkin/out/missing.json"))
                  |                        ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `train_call()`, called at lib/verify.star:1:5"#]],
    );
    check(
        "train_call",
        BodyKind::Job,
        json!({}),
        r#"
#- //lib/verify.star
def train_call(target, payload):
    return target.pt(target.get("/pumpkin/out/missing.json"), payload)
"#,
        &expect![[r#"
            lib/verify.star:2:19: `pt` is no method of a `Target`: call one of `caller`, `get`, `put`, `run`
                2 |     return target.pt(target.get("/pumpkin/out/missing.json"), payload)
                  |                   ^^
                the receiver, a `Target`, is made at lib/verify.star:1:5
                in `train_call()`, called at lib/verify.star:1:5"#]],
    );
    check(
        "train_call",
        BodyKind::Job,
        json!({}),
        r#"
#- //lib/verify.star
def train_call(target, payload):
    return target.put("/pumpkin/in/payload", "text")
"#,
        &expect![[r#"
            lib/verify.star:2:46: `put` can't put a string: give it the payload
                2 |     return target.put("/pumpkin/in/payload", "text")
                  |                                              ^^^^^^
                in `train_call()`, called at lib/verify.star:1:5"#]],
    );
    check(
        "train_call",
        BodyKind::Job,
        json!({}),
        r#"
#- //lib/verify.star
def train_call(target, payload):
    return target.run(["/opt/frontier/verifier", 1])
"#,
        &expect![[r#"
            lib/verify.star:2:23: `run` can't run an int: give it a list of strings
                2 |     return target.run(["/opt/frontier/verifier", 1])
                  |                       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                the argument, `1`, is made at lib/verify.star:2:50
                in `train_call()`, called at lib/verify.star:1:5"#]],
    );
    check(
        "submit",
        BodyKind::Now,
        json!({}),
        r#"
#- //lib/submit.star
def submit(run, payload):
    return payload.read()
"#,
        &expect![[r#"
            lib/submit.star:2:20: the payload has no method `read`: pass it to a `Target`'s `put` or a `Run`'s `set_submission`
                2 |     return payload.read()
                  |                    ^^^^
                the receiver, the payload, is made at lib/submit.star:1:5
                in `submit()`, called at lib/submit.star:1:5"#]],
    );
    check(
        "submit",
        BodyKind::Now,
        json!({}),
        r#"
#- //lib/submit.star
def submit(run, payload):
    run.select(payload)
"#,
        &expect![[r#"
            lib/submit.star:2:9: `select` is no method of a `Run`: call one of `end_work`, `set_submission`
                2 |     run.select(payload)
                  |         ^^^^^^
                the receiver, a `Run`, is made at lib/submit.star:1:5
                in `submit()`, called at lib/submit.star:1:5"#]],
    );
    check(
        "submit",
        BodyKind::Now,
        json!({}),
        r#"
#- //lib/submit.star
def submit(run, payload):
    run.set_submission("text")
"#,
        &expect![[r#"
            lib/submit.star:2:24: `set_submission` can't use a string: give it the payload
                2 |     run.set_submission("text")
                  |                        ^^^^^^
                in `submit()`, called at lib/submit.star:1:5"#]],
    );
    check(
        "submit",
        BodyKind::Now,
        json!({}),
        r#"
#- //lib/submit.star
def submit(run, payload):
    return {"run": run}
"#,
        &expect![[r#"
            lib/submit.star:2:12: `json.encode` can't encode a `Run`: give it `None`, bools, numbers and strings, and lists, tuples and dicts of them
                2 |     return {"run": run}
                  |            ^^^^^^^^^^^^
                the value, a `Run`, is made at lib/submit.star:1:5
                it is `run` of what `submit` returns, as JSON"#]],
    );
    check(
        "train_call",
        BodyKind::Job,
        json!({}),
        r#"
#- //lib/verify.star
def _verified(target: Target):
    pass

def train_call(target, payload):
    _verified(payload)
"#,
        &expect![[r#"
            lib/verify.star:5:5: `_verified`'s `target` can't be the payload: give a `Target`
                5 |     _verified(payload)
                  |     ^^^^^^^^^^^^^^^^^^
                the argument, the payload, is made at lib/verify.star:4:5
                in `train_call()`, called at lib/verify.star:4:5"#]],
    );
    check(
        "train_call",
        BodyKind::Job,
        json!({}),
        r#"
#- //lib/verify.star
def train_call(target, payload):
    target.put("pumpkin/in/payload", payload)
"#,
        &expect![[r#"
            lib/verify.star:2:16: `pumpkin/in/payload` is not a normalized absolute path: start with `/`, and use no empty, `.` or `..` components and no NUL byte
                2 |     target.put("pumpkin/in/payload", payload)
                  |                ^^^^^^^^^^^^^^^^^^^^
                in `train_call()`, called at lib/verify.star:1:5"#]],
    );
    check(
        "train_call",
        BodyKind::Job,
        json!({}),
        r#"
#- //lib/verify.star
def train_call(target, payload):
    target.run([])
"#,
        &expect![[r#"
            lib/verify.star:2:16: the command is empty: it needs a program
                2 |     target.run([])
                  |                ^^
                in `train_call()`, called at lib/verify.star:1:5"#]],
    );
    check(
        "submit",
        BodyKind::Now,
        json!({}),
        r#"
#- //lib/submit.star
def submit(run, payload):
    run.set_submission(payload)
"#,
        &expect![[r#"
            set the submission
            lib/submit.star:1:5: `submit` returns `None`, and a body returns a dict of names to values: return a dict
                1 | def submit(run, payload):
                  |     ^^^^^^"#]],
    );
    check(
        "submit",
        BodyKind::Now,
        json!({}),
        r#"
#- //lib/submit.star
def submit(run, payload):
    return {"state": "stored", "Bad name": 1}
"#,
        &expect![[r#"
            lib/submit.star:2:12: a key of the dict that `submit` returns is no name: `Bad name` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
                2 |     return {"state": "stored", "Bad name": 1}
                  |            ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                the key, `"Bad name"`, is made at lib/submit.star:2:32"#]],
    );
    check(
        "submit",
        BodyKind::Now,
        json!({}),
        r#"
#- //lib/submit.star
def submit(run, payload):
    return {1: "stored"}
"#,
        &expect![[r#"
            lib/submit.star:2:12: a key of the dict that `submit` returns is no name: `1` is no string: key it with a name
                2 |     return {1: "stored"}
                  |            ^^^^^^^^^^^^^
                the key, `1`, is made at lib/submit.star:2:13"#]],
    );
}

#[test]
fn bounds() {
    let bounds = Bounds {
        steps: 20,
        ..Bounds::WORKFLOW
    };
    check_within(
        bounds,
        "train_call",
        BodyKind::Job,
        json!({}),
        json!({}),
        r#"
#- //lib/verify.star
def train_call(target, payload):
    return [n for n in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10] if n]
"#,
        &expect![[r#"
            lib/verify.star:2:59: this evaluation runs past 20 steps, an expression's evaluation each: compute less, as with fewer `for` clauses or smaller lists
                2 |     return [n for n in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10] if n]
                  |                                                           ^
                in `train_call()`, called at lib/verify.star:1:5"#]],
    );
    let bounds = Bounds {
        bytes: 1024,
        ..Bounds::WORKFLOW
    };
    check_within(
        bounds,
        "train_call",
        BodyKind::Job,
        json!({}),
        json!({}),
        r#"
#- //lib/verify.star
def train_call(target, payload):
    text = target.get("/pumpkin/out/result.json")
    return text + text + text + text + text + text
"#,
        &expect![[r#"
            get /pumpkin/out/result.json
            lib/verify.star:3:38: this evaluation's strings, lists, tuples and dicts take more than 1024 bytes: make smaller values, as with fewer `+` of long ones
                3 |     return text + text + text + text + text + text
                  |                                      ^
                in `train_call()`, called at lib/verify.star:1:5"#]],
    );
}
