# Adds Qwen Code 0.25.0, Qwen's coding agent, as its npm package holds it, and the Node.js it runs
# on, to the image its build names `base`, at /harness/qwen-code-0.25.0 and /harness/node-24.14.0,
# in two layers of their own, laid out as `harness/codex/layer/Dockerfile` lays out Codex's: owned
# by root, read-only.
#
# The build's named context `node` is the image of `node()` in `harness/node/node.star`. The
# package is Qwen Code bundled, with the ripgrep
# its search runs, a static executable, and it needs none of its optional packages, which serve
# its interactive terminal, the clipboard, images and voice.

FROM node AS qwen-code-0.25.0
# The package's tarball from npm's registry, whose `sha512` integrity it also matches.
ADD --checksum=sha256:afeab0c85d682f201101319ce05decf3302d7ed7fb6319baa6a592051ea1a1ba https://registry.npmjs.org/@qwen-code/qwen-code/-/qwen-code-0.25.0.tgz /qwen-code.tgz
# The tarball gives ripgrep no executable bit.
RUN mkdir --parents /layer/harness/qwen-code-0.25.0 && tar --extract --gzip --file /qwen-code.tgz --directory /layer/harness/qwen-code-0.25.0 --strip-components=1 --no-same-owner && chmod --recursive a=rX /layer/harness && chmod 0555 /layer/harness/qwen-code-0.25.0/vendor/ripgrep/x64-linux/rg
FROM base
COPY --link --from=node --chown=0:0 /out/ /
COPY --link --from=qwen-code-0.25.0 --chown=0:0 /layer/ /
