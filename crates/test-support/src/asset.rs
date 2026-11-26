//! Files of an asset, each its path, its bytes and their SHA-256, as `sha256sum` prints it.

pub const CONFIG: (&str, &[u8], &str) = (
    "config.json",
    b"{}\n",
    "ca3d163bab055381827226140568f3bef7eaac187cebd76878e0b63e9e442356",
);

pub const SHARD: (&str, &[u8], &str) = (
    "shards/1.bin",
    b"1\n",
    "4355a46b19d348dc2f57c046f8ef63d4538ebb936000f3c9ee954a27460dd865",
);

pub const B: (&str, &[u8], &str) = (
    "b",
    b"b\n",
    "0263829989b6fd954f72baaf2fc64bc2e2f01d692d4de72986ea808f6e99813f",
);
