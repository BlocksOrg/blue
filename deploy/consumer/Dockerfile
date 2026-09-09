FROM busybox:1.37.0-musl
COPY --chown=1000:1000 provisioner.sh /executable/provisioner
RUN chmod 0555 /executable/provisioner
USER 1000:1000
