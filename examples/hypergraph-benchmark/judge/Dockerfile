FROM scratch AS wheels
ADD --checksum=sha256:5a56ba0db2d244117ed744dfa8f6f5b366e14148e00de44723413b2f3938a902 https://files.pythonhosted.org/packages/53/60/7be26e610767316c028a2cbedb9a3beabdbe33e2182c373f71a1c0b88f36/zstandard-0.25.0-cp312-cp312-manylinux2014_x86_64.manylinux_2_17_x86_64.whl /zstandard-0.25.0-cp312-cp312-manylinux2014_x86_64.manylinux_2_17_x86_64.whl

# The four graphs, which Ligra's randHypergraph makes from its parameters alone, checked by their
# SHA-256 digests, which receipts.py also holds.
FROM base AS workloads
RUN --mount=type=bind,from=ligra,target=/src cd /src/utils \
    && g++ -std=c++14 -O3 -I../ligra -o /tmp/randHypergraph randHypergraph.C \
    && mkdir /workloads \
    && /tmp/randHypergraph -nv 220000 -nh 360000 -c 10 /workloads/feedback-a.adj \
    && /tmp/randHypergraph -nv 300000 -nh 480000 -c 14 /workloads/feedback-b.adj \
    && /tmp/randHypergraph -nv 225000 -nh 355000 -c 10 /workloads/graded-a.adj \
    && /tmp/randHypergraph -nv 295000 -nh 485000 -c 14 /workloads/graded-b.adj \
    && printf '%s  %s\n' \
        47b51f021331da95098c8f11faa35c7c2bb617d69c838ed982b79f882e27dcdb /workloads/feedback-a.adj \
        5d97c2500eac5d07f5959d8436b61db3cf420e8e8e12bb09205ca7d4f799b740 /workloads/feedback-b.adj \
        5d90f176f2e912be3eb2836d0fd0f92b0b77bc367e2237facaa9b2dc0c82ad81 /workloads/graded-a.adj \
        5a684b49a471c6d37a6be35a16bdf044453fb01c81b0d9616ea41a3935478c87 /workloads/graded-b.adj \
        | sha256sum --check --strict

FROM base AS benchmark
RUN --mount=type=bind,from=wheels,target=/wheels apt-get update \
    && apt-get install -y --no-install-recommends python3-pip \
    && rm -rf /var/lib/apt/lists/* \
    && PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --no-index --no-deps /wheels/zstandard-0.25.0-cp312-cp312-manylinux2014_x86_64.manylinux_2_17_x86_64.whl \
    && install -d -o 65532 -g 65532 /scratch \
    && useradd --uid 65532 --user-group --no-create-home --home-dir /scratch --shell /bin/sh judge
# The reference is Ligra's own implementation, which the starter's stub replaces.
COPY --from=ligra /apps/hyper/HyperKCore-Efficient.C /opt/team/reference.C
COPY judge.py receipts.py verifier.py evaluator.py isolate.py /opt/team/
USER 65532:65532
ENV HOME=/scratch PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1
WORKDIR /scratch

# Training holds the feedback graphs alone.
FROM benchmark AS train
COPY --from=workloads /workloads/feedback-a.adj /workloads/feedback-b.adj /opt/team/workloads/
COPY train.json /opt/team/data/train.json

FROM benchmark AS eval
COPY --from=workloads /workloads/ /opt/team/workloads/
COPY eval.json /opt/team/data/eval.json

FROM eval AS replay
COPY replay.py /opt/team/
COPY selections /opt/replay/input/
