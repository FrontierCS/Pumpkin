//! `fmt`: `.star` files formatted in place by the real formatter beside `pumpkin`; with `--check`,
//! only said.

use std::fs;
use std::os::unix::fs::symlink;

use expect_test::expect;

use super::{Scratch, Step, check, ran};

#[test]
fn files() {
    let scratch = Scratch::new();
    let untidy = "load(\"//lib/x.star\",\"x\")\nY=[1,\n2]\ndef f( a ):\n  return a\n";
    scratch.write("lib/untidy.star", untidy);
    scratch.write("lib/tidy.star", "Y = [1, 2]\n");
    scratch.write("lib/notes.md", "Y=[1,\n");
    scratch.write("lib/.hidden/untidy.star", untidy);
    scratch.write("lib/.untidy.star", untidy);
    scratch.write("tasks/broken/task.star", "Y = (1,\nZ = 2\n");
    fs::create_dir(scratch.0.join("linked")).unwrap();
    symlink("../lib/untidy.star", scratch.0.join("linked/untidy.star")).unwrap();
    // Each runs in turn, as what one shows follows from those before it.
    let pumpkin = |args: &[&str]| Step::Ran(ran(scratch.pumpkin(args), &scratch.0, false));
    let shown = |path: &str| {
        let text = fs::read_to_string(scratch.0.join(path)).unwrap();
        Step::Ran(format!("$ cat {path}\n{text}"))
    };
    let steps = [
        pumpkin(&["fmt", "--check"]),
        pumpkin(&["fmt"]),
        shown("lib/untidy.star"),
        shown("lib/.hidden/untidy.star"),
        pumpkin(&["fmt", "--check", "lib", "linked/untidy.star"]),
        pumpkin(&["fmt", "lib/notes.md", "nowhere"]),
    ];
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            $ pumpkin fmt --check
            exit status: Some(1)
            pumpkin: lib/untidy.star: the file isn't formatted
            pumpkin: tasks/broken/task.star:2:4: the file isn't valid Starlark here, so it can't be formatted: make it valid Starlark
            pumpkin: to format them: pumpkin fmt <scratch>/lib/untidy.star <scratch>/tasks/broken/task.star
            $ pumpkin fmt
            exit status: Some(1)
            pumpkin: tasks/broken/task.star:2:4: the file isn't valid Starlark here, so it can't be formatted: make it valid Starlark
            pumpkin: to format it again: pumpkin fmt <scratch>/tasks/broken/task.star
            $ cat lib/untidy.star
            load("//lib/x.star", "x")

            Y = [
                1,
                2,
            ]

            def f(a):
                return a

            $ cat lib/.hidden/untidy.star
            load("//lib/x.star","x")
            Y=[1,
            2]
            def f( a ):
              return a

            $ pumpkin fmt --check lib linked/untidy.star
            exit status: Some(0)
            $ pumpkin fmt lib/notes.md nowhere
            exit status: Some(1)
            pumpkin: lib/notes.md is neither a `.star` file nor a directory: name one"#]],
    );
}
