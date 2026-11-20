FROM toolchain AS client
RUN mkdir --mode=1777 /workspace
COPY client.py /opt/team/

FROM client AS reference
COPY agent.py /opt/team/
COPY solver /opt/team/solver
