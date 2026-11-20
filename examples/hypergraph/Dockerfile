FROM python:3.10-slim@sha256:fd76ade0c607f27677bc04be3c60749f400eedc941d9e72967e19a4cedff80c2 AS graph
RUN mkdir --mode=1777 /workspace
COPY graph.json /opt/hypergraph/

FROM graph AS reference
COPY reference.py /opt/hypergraph/

FROM graph AS judge
COPY judge.py /opt/hypergraph/
