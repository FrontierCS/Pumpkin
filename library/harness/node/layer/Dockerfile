# Node.js 24.14.0's official image for x86-64 Linux, with the tree that adds Node to an agent's
# image below /out: /out/harness/node-24.14.0, owned by root, read-only, laid out as
# `harness/codex/layer/Dockerfile` lays out Codex's.
#
# `node` links glibc, so the tree carries the glibc it runs on, in `lib`, and `node` names it as its
# interpreter and run path, for the reason `harness/claude_code/layer/Dockerfile` gives. It starts
# as itself because Node gives its own executable's path as `process.execPath`, from which a
# harness starts its workers and child processes, and a loader started in its place would take
# their arguments for its own.

# Node 24.14.0 on Debian 12, with glibc 2.36, by the digest of its index, which names an image for
# each platform.
FROM node@sha256:d8e448a56fc63242f70026718378bd4b00f8c82e78d20eefb199224a4d8e33d8
# PatchELF 0.19.3, the release the Claude Code layer fetches.
ADD --checksum=sha256:38861b171079e440b25ac86483284f690d0ee7921dba98b0c8fe04a9b0a11667 https://github.com/NixOS/patchelf/releases/download/0.19.3/patchelf-0.19.3-x86_64.tar.gz /patchelf.tgz
# The libraries are those `node` names as needed (`libdl`, `libstdc++`, `libm`, `libgcc_s`,
# `libpthread`, `libc`) and the loader, which `libc` needs too.
RUN mkdir /patchelf && tar --extract --gzip --file /patchelf.tgz --directory /patchelf ./bin/patchelf && mkdir --parents /out/harness/node-24.14.0/lib && cp /usr/local/bin/node /usr/local/LICENSE /out/harness/node-24.14.0/ && /patchelf/bin/patchelf --set-interpreter /harness/node-24.14.0/lib/ld-linux-x86-64.so.2 --set-rpath /harness/node-24.14.0/lib /out/harness/node-24.14.0/node && rm --recursive /patchelf /patchelf.tgz && cd /lib/x86_64-linux-gnu && cp --dereference ld-linux-x86-64.so.2 libc.so.6 libdl.so.2 libgcc_s.so.1 libm.so.6 libpthread.so.0 libstdc++.so.6 /out/harness/node-24.14.0/lib/ && cd /out/harness/node-24.14.0 && chmod 0444 LICENSE lib/libc.so.6 lib/libdl.so.2 lib/libgcc_s.so.1 lib/libm.so.6 lib/libpthread.so.0 lib/libstdc++.so.6 && chmod 0555 /out/harness . lib node lib/ld-linux-x86-64.so.2
