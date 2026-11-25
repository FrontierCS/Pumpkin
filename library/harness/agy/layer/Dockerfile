# Adds agy 1.5.16, Google's Antigravity CLI, as its release holds it, to the image its build names
# `base`, at /harness/agy-1.5.16, in a layer of its own, laid out as
# `harness/codex/layer/Dockerfile` lays out Codex's: owned by root, read-only.
#
# `antigravity` links glibc, so the layer carries the glibc it runs on, in `lib`, and
# `antigravity` names it as its interpreter and run path, for the reason
# `harness/claude_code/layer/Dockerfile` gives.

# Debian 12, with glibc 2.36, by the digest of its index, which names an image for each platform.
FROM debian:bookworm-slim@sha256:7ff6ba31527b9e336162fa061746c366c4b2032f4d2cff2078bea43f78efe3f8 AS agy-1.5.16
# The release for x86-64 Linux that agy's installer fetches.
ADD --checksum=sha256:9bdab234d06bad1a61be2321072652eb06be78baff1acbfae47331da5a090abc https://storage.googleapis.com/antigravity-public/antigravity-cli/1.5.16-5250855658717184/linux-x64/cli_linux_x64.tar.gz /agy.tgz
# Debian 12's PatchELF 0.14.3, because PatchELF 0.18 or later writes an `antigravity` that fails as
# it starts. snapshot.debian.org keeps the file at this URL, which the pool's path doesn't once
# bookworm moves to the archive.
ADD --checksum=sha256:0364b90e81faabbb9569076063952c7bcbf07f83d954a2f2602a163d65be60e6 https://snapshot.debian.org/archive/debian/20220919T001659Z/pool/main/p/patchelf/patchelf_0.14.3-1+b1_amd64.deb /patchelf.deb
# The libraries are those `antigravity` names as needed (`libdl`, `libm`, `libpthread`,
# `libresolv`, `librt`, `libc`) and the loader, which `libc` needs too.
RUN mkdir --parents /layer/harness/agy-1.5.16/lib /patchelf && tar --extract --gzip --file /agy.tgz --directory /layer/harness/agy-1.5.16 --no-same-owner antigravity && dpkg --extract /patchelf.deb /patchelf && /patchelf/usr/bin/patchelf --set-interpreter /harness/agy-1.5.16/lib/ld-linux-x86-64.so.2 --set-rpath /harness/agy-1.5.16/lib /layer/harness/agy-1.5.16/antigravity && cd /lib/x86_64-linux-gnu && cp --dereference ld-linux-x86-64.so.2 libc.so.6 libdl.so.2 libm.so.6 libpthread.so.0 libresolv.so.2 librt.so.1 /layer/harness/agy-1.5.16/lib/
RUN cd /layer/harness/agy-1.5.16 && chmod 0444 lib/libc.so.6 lib/libdl.so.2 lib/libm.so.6 lib/libpthread.so.0 lib/libresolv.so.2 lib/librt.so.1 && chmod 0555 /layer/harness . lib antigravity lib/ld-linux-x86-64.so.2
FROM base
COPY --link --from=agy-1.5.16 --chown=0:0 /layer/ /
