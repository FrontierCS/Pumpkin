//! The gateway's ports, which `deploy/aws` admits from the sandboxes, as
//! `deploy/aws/gateway_ports.json` holds them, which this test keeps equal to the code's
//! (`ports`, which this binary compiles itself, as the crate keeps it private).

use expect_test::expect_file;
use serde_json::json;

#[path = "../../src/host/gateway_ports.rs"]
mod ports;

use ports::GATEWAY_PORTS;

#[test]
fn gateway_ports() {
    let ports = json!({ "from": GATEWAY_PORTS.start(), "to": GATEWAY_PORTS.end() });
    expect_file!["../../../../deploy/aws/gateway_ports.json"].assert_eq(&format!("{ports:#}\n"));
}
