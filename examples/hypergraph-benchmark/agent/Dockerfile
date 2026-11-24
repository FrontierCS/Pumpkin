FROM base
RUN useradd --create-home --uid 10001 --user-group --shell /bin/sh agent \
    && install -d -o 10001 -g 10001 /workspace
COPY start.py /opt/team/
# Python finds edit_scope beside the link's target.
COPY --chmod=0555 archive.py /opt/team/
RUN ln -s /opt/team/archive.py /usr/local/bin/make-team-archive
USER 10001:10001
ENV HOME=/workspace TMPDIR=/workspace PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1
WORKDIR /workspace
