ARG BASE
FROM ${BASE} AS published
USER root
COPY binding/LICENSE binding/ATTRIBUTION /usr/share/doc/hypergraph-team/
COPY binding/interface.json judge.py receipts.py /opt/team/
ENV PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1

FROM published AS train_files
COPY binding/train.json /opt/team/data/train.json
RUN python3 -c 'import json, pathlib; [pathlib.Path(p).unlink() for p in json.load(open("/opt/team/interface.json"))["withheld"]]'

FROM scratch AS train
COPY --from=train_files / /
ENV PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1
USER 65532:65532
WORKDIR /pumpkin/home

FROM published AS eval
COPY binding/eval.json /opt/team/data/eval.json
USER 65532:65532
WORKDIR /pumpkin/home

FROM eval AS replay
COPY replay.py /opt/team/
COPY selections /opt/replay/input/
