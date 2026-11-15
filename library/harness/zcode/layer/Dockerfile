# Adds ZCode 0.16.9, Z.ai's coding agent, built from its source at the commit of that release, and
# the Node.js it runs on, to the image its build names `base`, at /harness/zcode-0.16.9 and
# /harness/node-24.14.0, in two layers of their own, laid out as `harness/codex/layer/Dockerfile`
# lays out Codex's: owned by root, read-only.
#
# The build's named context `node` is the image of `node()` in `harness/node/node.star`, in which
# ZCode is built. Of the build it keeps
# `zcode.cjs`, ZCode's command-line program bundled into one file, which `node` runs; `provider`,
# the catalog of the providers it knows, which it reads at start; `LICENSE`, ZCode's terms, and
# `THIRD-PARTY-NOTICES.md`, those of what the bundle holds.

FROM node AS zcode-0.16.9
ADD https://github.com/zai-org/ZCode.git#29628c9acdb81b703bbd4080c207a0e7ce5e276e /source
# pnpm 10.33.2, the package manager whose lockfile pins each package ZCode is built from.
ADD --checksum=sha256:7a7bcf13d7f6ceb3946c03978373d99be9fde1cafc3000bdfed4c4f791167610 https://registry.npmjs.org/pnpm/-/pnpm-10.33.2.tgz /pnpm.tgz
# Installs, as the lockfile pins them and running none of their scripts, the packages that the
# command-line program needs, and builds them in dependency order, each with its own build script.
RUN npm install --global --offline /pnpm.tgz && cd /source && pnpm install --frozen-lockfile --ignore-scripts --filter @zcode/cli... && pnpm --filter @zcode/cli... run build
RUN mkdir --parents /layer/harness/zcode-0.16.9 && cd /source && cp LICENSE apps/zcode-cli/packages/cli/dist/zcode.cjs apps/zcode-cli/packages/cli/dist/THIRD-PARTY-NOTICES.md /layer/harness/zcode-0.16.9/ && cp --recursive apps/zcode-cli/packages/cli/dist/provider /layer/harness/zcode-0.16.9/ && chmod --recursive a=rX /layer/harness
FROM base
COPY --link --from=node --chown=0:0 /out/ /
COPY --link --from=zcode-0.16.9 --chown=0:0 /layer/ /
