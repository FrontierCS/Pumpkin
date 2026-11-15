"""Node.js, the JavaScript runtime that harnesses written in JavaScript run on: `node` gives the
image that carries Node 24.14.0 for a harness's layer to copy, and the path its `node` has there.

The image is Node's official Debian 12 image, which a harness's recipe also builds the harness in,
with the tree a layer copies below `/out`: `/out/harness/node-24.14.0`, holding `node`, the
glibc it runs on, in `lib`, and Node's licence. A recipe takes the image as a named context and
copies that tree to `/` in a layer of its own, `COPY --link --from=<context> --chown=0:0 /out/ /`,
which is then the same on any base. `node` runs on any base, as itself, so a child it starts from
its own path, as a harness's worker or sub-process is, runs on the same glibc.
"""

# Where an agent's image holds `node`, once a layer copies the image's tree.
NODE = "/harness/node-24.14.0/node"

def node():
    """The image that carries Node 24.14.0, which `image()` makes, for a recipe's named context."""
    return image("Dockerfile", context = "@pumpkin//harness/node/layer")
