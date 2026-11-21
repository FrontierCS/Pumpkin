# Adds Pi 1.2.0, Earendil's coding agent, with the packages it runs on, and the Node.js it runs on,
# to the image its build names `base`, at /harness/pi-1.2.0 and /harness/node-24.14.0, in two
# layers of their own, laid out as `harness/codex/layer/Dockerfile` lays out Codex's: owned by
# root, read-only.
#
# The build's named context `node` is the image of `node()` in `harness/node/node.star`, whose npm
# installs Pi. `package-lock.json`
# pins each package by its version and the SHA-512 of its tarball; npm made it for
# `package.json` with `npm install --package-lock-only --before=2026-10-14T00:00:00Z` to
# limit resolution to versions published before the cutoff.

FROM node AS pi-1.2.0
COPY package.json package-lock.json /layer/harness/pi-1.2.0/
# Installs the packages as the lockfile pins them, running none of their scripts, which Pi needs
# none of.
RUN cd /layer/harness/pi-1.2.0 && npm ci --ignore-scripts --no-audit --no-fund && chmod --recursive a=rX /layer/harness
FROM base
COPY --link --from=node --chown=0:0 /out/ /
COPY --link --from=pi-1.2.0 --chown=0:0 /layer/ /
