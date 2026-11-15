# Adds Claude Code 2.1.296 for x86-64 Linux, from npm's platform tarball, to the image its build
# names `base`, at /harness/claude-code-2.1.296, in a layer of its own, laid out as
# `harness/codex/layer/Dockerfile` lays out Codex's: owned by root, read-only.
#
# Of the tarball it keeps `claude`, Claude Code itself, one executable that holds its own runtime,
# and `LICENSE.md`, Anthropic's terms for it. `claude` links glibc, which an agent's image may lack
# or hold too old a version of, so the layer carries the glibc it runs on: Debian's dynamic loader
# and the libraries `claude` needs, in `lib`. `claude` names that loader as its interpreter and
# that directory as its run path, as Nix patches the same executable
# (https://github.com/NixOS/nixpkgs/blob/06c561b7cdfb8ea2f98ac13811a4f2da5f16f931/pkgs/by-name/cl/claude-code/package.nix#L36-L45),
# so that it starts as itself: Claude Code runs its own executable again, under other names, as
# the `find` and `grep` of the agent's shell, which a loader started in its place would take for
# its own arguments.

# Debian 12's slim image of 2026-10-27, with glibc 2.36, by the digest of its index, which names
# an image for each platform: its `tar` unpacks the tarballs, which `ADD --checksum` fetches and
# checks, and its glibc is the one the layer carries.
FROM debian@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3 AS claude-code-2.1.296
ADD --checksum=sha256:eef5a2e2b09a5e7d2ce784d1b3ba335ae7fc6c7f6256c802a0ebf2c21e4fd0e1 https://registry.npmjs.org/@anthropic-ai/claude-code-linux-x64/-/claude-code-linux-x64-2.1.296.tgz /harness.tgz
# PatchELF 0.19.3's static executable, from its release, which sets the interpreter and run path.
ADD --checksum=sha256:38861b171079e440b25ac86483284f690d0ee7921dba98b0c8fe04a9b0a11667 https://github.com/NixOS/patchelf/releases/download/0.19.3/patchelf-0.19.3-x86_64.tar.gz /patchelf.tgz
# This stage stages the tree the image gets below /out, with each file's mode, and one `COPY
# --link` copies it, for the reason `harness/codex/layer/Dockerfile` gives. The libraries are those
# `claude` names as needed (`librt`, `libc`, `libpthread`, `libdl`, `libm`) and the loader, which
# `libc` needs too.
RUN tar --extract --gzip --file /harness.tgz --directory / package/claude package/LICENSE.md && mkdir /patchelf && tar --extract --gzip --file /patchelf.tgz --directory /patchelf ./bin/patchelf && /patchelf/bin/patchelf --set-interpreter /harness/claude-code-2.1.296/lib/ld-linux-x86-64.so.2 --set-rpath /harness/claude-code-2.1.296/lib /package/claude && mkdir --parents /out/harness/claude-code-2.1.296/lib && mv /package/claude /package/LICENSE.md /out/harness/claude-code-2.1.296/ && cd /lib/x86_64-linux-gnu && cp --dereference ld-linux-x86-64.so.2 libc.so.6 libdl.so.2 libm.so.6 libpthread.so.0 librt.so.1 /out/harness/claude-code-2.1.296/lib/ && cd /out/harness/claude-code-2.1.296 && chmod 0444 LICENSE.md lib/libc.so.6 lib/libdl.so.2 lib/libm.so.6 lib/libpthread.so.0 lib/librt.so.1 && chmod 0555 /out/harness . lib claude lib/ld-linux-x86-64.so.2
FROM base
COPY --link --from=claude-code-2.1.296 --chown=0:0 /out/ /
