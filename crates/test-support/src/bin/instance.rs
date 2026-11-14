//! A stand-in for the commands that `ec2-boot` runs on an EC2 sandbox's instance, `docker`, `nft`
//! and `shutdown`, for its tests, which each find it on `PATH` through links to it, each named as
//! a command, in a directory of their own. It answers as each would on an instance whose Docker
//! is 29.1.3 and supports every limit, and records in that directory each call, in `calls`, its
//! command and then its arguments, a line each, and the call ended by an empty line, as the
//! stand-in `aws` does. `nft` keeps what it reads in `nft-rules`; `docker cp` writes the path it
//! copies from to the file it copies to; `docker run` writes, to `env-file`, the mode and the text
//! of the file that its `--env-file` names. A file there named as a command and its first
//! argument, such as `docker version`, is printed in place of the answer below, or, if its first
//! line is `fail`, the rest is printed to standard error, and the call fails.

use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use stdx::Context as _;

fn main() -> ExitCode {
    let args: Vec<String> = env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            let _ = writeln!(io::stderr(), "{message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[&str]) -> Result<(), String> {
    let name = env::args_os().next().unwrap_or_default();
    let name = Path::new(&name).file_name().unwrap_or_default().to_owned();
    let dir = directory(&name)?;
    let name = name.to_string_lossy().into_owned();
    record(&dir, &name, args)?;
    let first = args.first().copied().unwrap_or_default();
    if let Ok(given) = fs::read_to_string(dir.join(format!("{name} {first}"))) {
        return match given.strip_prefix("fail\n") {
            Some(error) => Err(error.trim_end().to_owned()),
            None => print(&given),
        };
    }
    let said = |text: &str| print(&format!("{text}\n"));
    match (name.as_str(), first) {
        ("nft", _) => {
            let mut rules = String::new();
            io::stdin()
                .read_to_string(&mut rules)
                .context("reading the rules")?;
            fs::write(dir.join("nft-rules"), rules).context("keeping the rules")
        }
        ("shutdown", _) | ("docker", "pull") => Ok(()),
        ("docker", "info") => said("true true true true"),
        ("docker", "version") => said("29.1.3"),
        // The reference by digest that the image was pulled by.
        ("docker", "image") => said(args.last().copied().unwrap_or_default()),
        ("docker", "logout") => said(&format!("Removing login credentials for {}", arg(args, 1))),
        ("docker", "create") => said("c0ffee"),
        ("docker", "cp") => {
            let copied = format!("copied from {}\n", arg(args, 1));
            fs::write(arg(args, 2), copied).context("copying")
        }
        ("docker", "rm") => said(arg(args, 1)),
        ("docker", "run") => {
            let file = args.iter().find_map(|arg| arg.strip_prefix("--env-file="));
            let file = file.ok_or("the stand-in: `docker run` names no `--env-file`")?;
            let mode = fs::metadata(file).context(file)?.permissions().mode() & 0o7777;
            let text = fs::read_to_string(file).context(file)?;
            let kept = format!("mode {mode:04o}: {text}");
            fs::write(dir.join("env-file"), kept).context("keeping the env file")?;
            said("5f3c")
        }
        ("docker", "wait") => said("0"),
        ("docker", "logs") => {
            let _ = writeln!(io::stderr(), "a line on standard error");
            said("a line on standard output")
        }
        _ => Err(format!(
            "the stand-in doesn't know `{name} {}`",
            args.join(" ")
        )),
    }
}

/// The `index`th of `args`, or nothing.
fn arg<'a>(args: &[&'a str], index: usize) -> &'a str {
    args.get(index).copied().unwrap_or_default()
}

fn print(text: &str) -> Result<(), String> {
    io::stdout()
        .write_all(text.as_bytes())
        .context("standard output")
}

/// The directory of the first `name` on `PATH` that is a link to this program.
fn directory(name: &std::ffi::OsStr) -> Result<PathBuf, String> {
    let exe = env::current_exe().context("the stand-in's own path")?;
    let path = env::var_os("PATH").unwrap_or_default();
    let linked = |dir: &PathBuf| fs::canonicalize(dir.join(name)).is_ok_and(|found| found == exe);
    (env::split_paths(&path).find(linked)).ok_or_else(|| {
        format!(
            "`PATH` names no directory with a link to {}, named {}: put one first on it",
            exe.display(),
            Path::new(name).display()
        )
    })
}

/// Appends the call, of command `name` with `args`, to `dir`'s `calls`.
fn record(dir: &Path, name: &str, args: &[&str]) -> Result<(), String> {
    let mut call = OsString::from(name);
    call.push("\n");
    for arg in args {
        call.push(arg);
        call.push("\n");
    }
    call.push("\n");
    let calls = OpenOptions::new()
        .append(true)
        .create(true)
        .open(dir.join("calls"));
    let mut calls = calls.context("opening `calls`")?;
    calls.write_all(call.as_bytes()).context("writing `calls`")
}
