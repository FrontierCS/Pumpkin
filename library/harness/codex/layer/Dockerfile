# Adds Codex 0.167.0 for x86-64 Linux, from npm's platform tarball, to the image its build names
# `base`, at /harness/codex-0.167.0, in a layer of its own, independent of that image, all owned by
# root: each file read-only, 0444, or read-only and executable, 0555, in directories of mode 0555.
#
# Of the tarball it keeps `bin/codex`, Codex itself, which finds its other files from its own
# executable's path: `codex-package.json`, beside its `bin` directory, marks the package's layout.
# `bin/codex-code-mode-host` is the process Codex starts for code mode, and `codex-path/rg` is
# ripgrep, which Codex puts on its tools' `PATH`. Every binary is static, so Codex needs nothing
# from the image it is added to. `codex-resources` stays out: its `bwrap` is Codex's own sandbox,
# for which the agent's sandbox stands in, and its voice host serves spoken input, which an
# unattended agent has none of.

# busybox 1.37, by the digest of its index, which names an image for each platform: its `tar`
# unpacks the tarball, which `ADD --checksum` fetches and checks, so the engine needs Docker 25.0
# or later.
FROM busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e AS codex-0.167.0
ADD --checksum=sha256:2dc9aa871abb513c3539d20d025f701faf1974c46ee906bda3a1e1f5026d4434 https://registry.npmjs.org/@openai/codex/-/codex-0.167.0-linux-x64.tgz /harness.tgz
RUN tar --extract --gzip --file /harness.tgz --directory / package/vendor/x86_64-unknown-linux-musl/bin/codex package/vendor/x86_64-unknown-linux-musl/bin/codex-code-mode-host package/vendor/x86_64-unknown-linux-musl/codex-package.json package/vendor/x86_64-unknown-linux-musl/codex-path/rg
# A `COPY` gives each file and directory it copies the permission bits it has where it is copied
# from, unless `--chmod` gives them all one, but the directories it makes for its destination get
# bits that differ from engine to engine. So this stage puts the files in the tree the image gets,
# below /out, and gives each file and directory its mode, and one `COPY` copies that tree to `/`,
# which every image has, with `--link`, so that an engine that can reuses the layer on any base
# rather than copying it again.
FROM busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e AS codex-0.167.0-tree
COPY --from=codex-0.167.0 /package/vendor/x86_64-unknown-linux-musl/ /out/harness/codex-0.167.0/
RUN chmod 0444 /out/harness/codex-0.167.0/codex-package.json && chmod 0555 /out/harness /out/harness/codex-0.167.0 /out/harness/codex-0.167.0/bin /out/harness/codex-0.167.0/bin/codex /out/harness/codex-0.167.0/bin/codex-code-mode-host /out/harness/codex-0.167.0/codex-path /out/harness/codex-0.167.0/codex-path/rg
FROM base
COPY --link --from=codex-0.167.0-tree --chown=0:0 /out/ /
