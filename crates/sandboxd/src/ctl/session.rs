//! A session's ops, as `sandboxctl call <session>.<op>` gives them.

use std::ffi::{OsStr, OsString};
use std::num::NonZeroU64;
use std::path::Component;

use schema::ir::{AbsolutePath, Argv, CompanionName, Name};
use schema::sandbox::op::{OpCall, SessionOp};

/// The op of session `grant` that `op` names, with `args`, the op's arguments, and `timeout`,
/// given for `run` alone, `seconds`, for `open` alone, `replace`, for `put` alone, and
/// `companion`, for `run`, `put` and `get`.
pub(super) fn op(
    (grant, op): (&str, &str),
    args: &[OsString],
    (timeout, seconds, replace, companion): (Option<&OsStr>, Option<&OsStr>, bool, Option<&OsStr>),
) -> Result<OpCall, String> {
    let grant = Name::try_from(grant.to_owned())?;
    let text = |arg: &OsStr| {
        (arg.to_str().map(str::to_owned)).ok_or_else(|| {
            format!(
                "`{}` is not UTF-8: give each argument as UTF-8",
                arg.display()
            )
        })
    };
    let args: Vec<String> = args.iter().map(|arg| text(arg)).collect::<Result<_, _>>()?;
    let number = |lease: &str| {
        lease.parse().map_err(|_| {
            format!("`{lease}` is not a lease: name one by the number `{grant}.open` returned")
        })
    };
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let container = (companion.map(|companion| text(companion).and_then(CompanionName::try_from)))
        .transpose()?;
    let single = container.is_none();
    let session = match (op, &words[..], timeout, seconds, replace) {
        ("open", [], None, _, false) if single => SessionOp::Open {
            instance_seconds: seconds
                .map(|seconds| whole(seconds, "a lease's time"))
                .transpose()?,
        },
        ("run", [lease, command @ ..], _, None, false) if !command.is_empty() => SessionOp::Run {
            lease: number(lease)?,
            argv: Argv::try_from(
                command
                    .iter()
                    .map(|&arg| arg.to_owned())
                    .collect::<Vec<_>>(),
            )?,
            timeout_seconds: timeout
                .map(|timeout| whole(timeout, "a timeout"))
                .transpose()?,
            container,
        },
        ("put", [lease, tree, to], None, None, _) => SessionOp::Put {
            lease: number(lease)?,
            tree: here(tree)?,
            to: there(to)?,
            replace,
            container,
        },
        ("get", [lease, from, to], None, None, false) => SessionOp::Get {
            lease: number(lease)?,
            from: there(from)?,
            to: here(to)?,
            container,
        },
        ("status", [lease], None, None, false) if single => SessionOp::Status {
            lease: number(lease)?,
        },
        ("close", [lease], None, None, false) if single => SessionOp::Close {
            lease: number(lease)?,
        },
        _ => {
            let words = core::iter::once(format!("{grant}.{op}")).chain(args);
            return Err(format!(
                "`{}` is no session op: give `{grant}.open [--seconds <seconds>]`, `{grant}.run \
                 <lease> [--timeout <seconds>] [--companion <name>] -- <program> [<arg>...]`, \
                 `{grant}.put <lease> <tree> <to> [--replace] [--companion <name>]`, \
                 `{grant}.get <lease> <from> <to> [--companion <name>]`, `{grant}.status \
                 <lease>` or `{grant}.close <lease>`",
                words.collect::<Vec<_>>().join(" ")
            ));
        }
    };
    Ok(OpCall::Session { grant, op: session })
}

/// `path`, a path in this sandbox, whole: from the current directory, unless it is whole
/// already, and with each `..` taking the component before it away, as the path need not exist.
fn here(path: &str) -> Result<AbsolutePath, String> {
    let whole = std::path::absolute(path).map_err(|error| format!("{path}: {error}"))?;
    let mut parts = Vec::new();
    for part in whole.components() {
        match part {
            Component::Normal(part) => {
                let part = part.to_str().ok_or_else(|| {
                    format!("{path} is not UTF-8 as a whole path: give one that is")
                })?;
                parts.push(part);
            }
            Component::ParentDir => {
                parts.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
        }
    }
    AbsolutePath::try_from(format!("/{}", parts.join("/")))
}

/// `path`, a path in a lease's runner, given whole, as no directory there is current here.
fn there(path: &str) -> Result<AbsolutePath, String> {
    if !path.starts_with('/') {
        return Err(format!(
            "`{path}` is a path in the runner, which needs a whole one: start it with `/`"
        ));
    }
    AbsolutePath::try_from(path.to_owned())
}

/// The seconds that `seconds`, `what` they are, gives, a positive whole number.
fn whole(seconds: &OsStr, what: &str) -> Result<NonZeroU64, String> {
    (seconds.to_str().and_then(|seconds| seconds.parse().ok())).ok_or_else(|| {
        format!(
            "`{}` is not {what}: give a whole number of seconds, 1 or more",
            seconds.display()
        )
    })
}
