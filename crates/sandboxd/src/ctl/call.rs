//! `sandboxctl call`: the op that calls a function that the caller's role holds.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

use schema::ir::{Name, argument};
use schema::sandbox::op::OpCall;

use super::{flags, parse_grant, payload_path, session};

/// The op that `call` makes, and the request's body: a session's op where the function's name
/// holds a dot, after which the op's name follows, and otherwise the call of the function with
/// the payload in the one file that `call` gives.
pub(super) fn call(call: flags::Call) -> Result<(OpCall, Vec<u8>), String> {
    let flags::Call {
        function,
        args,
        timeout,
        seconds,
        replace,
        wait,
        arg,
    } = call;
    if let Some(op) = function
        .to_str()
        .and_then(|function| function.split_once('.'))
    {
        if wait.is_some() {
            return Err(format!(
                "a session's `{}` takes no `--wait`: give its own words and flags",
                op.1
            ));
        }
        if !arg.is_empty() {
            return Err(format!(
                "a session's `{}` takes no `--arg`: give its own words and flags",
                op.1
            ));
        }
        let flags = (timeout.as_deref(), seconds.as_deref(), replace);
        return Ok((session::op(op, &args, flags)?, Vec::new()));
    }
    let function = parse_grant(&function)?;
    match (&args[..], timeout, seconds, replace) {
        (files @ ([] | [_]), None, None, false) => {
            let arguments = arguments(&arg)?;
            let call = OpCall::Call {
                grant: function,
                arguments,
            };
            let body = files
                .first()
                .map(|file| payload_path(Path::new(file)))
                .transpose()?
                .unwrap_or_default();
            Ok((call, body))
        }
        _ => Err(format!(
            "`{function}` takes at most one payload file: call it \
             as `sandboxctl call {function} [<file>] [--arg <name>=<value>]… [--wait <seconds>]`"
        )),
    }
}

// The caller's arguments that `given` writes, each `--arg <name>=<value>`, by name.
fn arguments(given: &[OsString]) -> Result<BTreeMap<Name, String>, String> {
    let given = (given.iter())
        .map(|text| {
            text.to_str()
                .ok_or_else(|| format!("`--arg {}` is not UTF-8: use UTF-8", text.display()))
        })
        .collect::<Result<Vec<&str>, String>>()?;
    let texts = argument::texts(given).map_err(|problems| problems.join("; "))?;
    (texts.into_iter())
        .map(|(name, text)| Ok((Name::try_from(name)?, text)))
        .collect()
}
