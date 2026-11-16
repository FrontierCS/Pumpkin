//! `sandboxctl call`: the op that calls a function that the caller's role holds.

use std::path::Path;

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
    } = call;
    if let Some(op) = function
        .to_str()
        .and_then(|function| function.split_once('.'))
    {
        let flags = (timeout.as_deref(), seconds.as_deref(), replace);
        return Ok((session::op(op, &args, flags)?, Vec::new()));
    }
    let function = parse_grant(&function)?;
    match (&args[..], timeout, seconds, replace) {
        ([file], None, None, false) => Ok((
            OpCall::Call { grant: function },
            payload_path(Path::new(file))?,
        )),
        _ => Err(format!(
            "`{function}` takes the file that holds the payload, and no flag: call it as \
             `sandboxctl call {function} <file>`"
        )),
    }
}
