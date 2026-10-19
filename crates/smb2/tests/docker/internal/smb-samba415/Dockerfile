# Samba 4.15, deliberately old: the last series that refuses a FLUSH inside a
# compound (MS-SMB2 § 3.3.5.2.7) instead of running it synchronously, and the
# refusal frees the request while its fsync is still running, which panics
# smbd (Samba bug 15172, fixed in 4.16.x, 4.17.x, and 4.18). Older NAS
# firmware ships this series. ❌ Don't bump the base image: the old Samba is
# the point.
FROM alpine:3.15
RUN apk add --no-cache samba
COPY smb.conf /etc/samba/smb.conf
RUN mkdir -p /shares/public && chmod 777 /shares/public
EXPOSE 445
HEALTHCHECK --interval=2s --timeout=3s --retries=10 \
    CMD nc -z localhost 445
CMD ["smbd", "--foreground", "--no-process-group", "--debug-stdout"]
