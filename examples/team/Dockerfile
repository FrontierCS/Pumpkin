FROM toolchain AS client
RUN mkdir --mode=1777 /workspace

FROM client AS reference
COPY agent.py /opt/team/
COPY solver /opt/team/solver
