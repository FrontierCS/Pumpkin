# Pumpkin

## Contributing

[`AGENTS.md`](AGENTS.md) holds the rules for changing the code, and `cargo xtask ci` the
checks a change passes; `cargo xtask live docker` runs the tests that need a Docker engine, and
`cargo xtask live ec2` and `cargo xtask live modal` a smoke run through your controller.

The smoke run reads your controller from `~/.config/pumpkin/pumpkin.toml`: its `[controller]`
table, which `deploy/aws` made, and, on Modal, its `[controller.modal]`, the workspace's profile
and classes, as [`deploy/aws/README.md`](deploy/aws/README.md) says.
