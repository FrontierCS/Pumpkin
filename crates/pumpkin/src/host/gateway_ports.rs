//! The ports that a job's gateway listens on ([`GATEWAY_PORTS`]). deploy/aws admits them from
//! the sandboxes, as `deploy/aws/gateway_ports.json` holds them, which `pumpkin`'s tests keep
//! equal to these.

use std::ops::RangeInclusive;

/// The ports that a job's gateway listens on, on port 0. They are Linux's default range of
/// ephemeral ports. The install requires the host's range to be within them.
// https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/net/ipv4/af_inet.c#L1805
// https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/Documentation/networking/ip-sysctl.rst#L1341-L1348
pub(crate) const GATEWAY_PORTS: RangeInclusive<u16> = 32768..=60999;
