ARG BASE
FROM ${BASE}
USER root
COPY binding/LICENSE binding/ATTRIBUTION /usr/share/doc/hypergraph-team/
COPY binding/interface.json start.py /opt/team/
COPY --chmod=0555 archive.py /usr/local/bin/make-team-archive
USER 10001:10001
