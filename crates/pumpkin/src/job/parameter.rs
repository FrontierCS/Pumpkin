//! Reads SSM parameters. The controller's settings and its members' list are the values it reads.

use aws::iam::Call;
use aws::{Bound, Cli};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GotParameter {
    parameter: Parameter,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Parameter {
    value: String,
}

/// The value of SSM parameter `name`. `cli` reads it as `call` declares.
pub(crate) fn parameter_value(
    cli: &Cli,
    call: &'static Call,
    name: &str,
) -> Result<String, String> {
    let got: GotParameter = cli.json(call, &["--name", name], Bound::Call)?;
    Ok(got.parameter.value)
}
