#!/bin/sh
# Starts as root only to fix ownership of the data directory (bind mounts and
# older volumes are often root-owned), then drops to the unprivileged user.
set -e
if [ "$(id -u)" = "0" ]; then
  mkdir -p "$DATA_DIR"
  chown wuc "$DATA_DIR" 2>/dev/null || true
  find "$DATA_DIR" -xdev ! -user wuc -exec chown wuc {} + 2>/dev/null || true
  exec setpriv --reuid=wuc --regid=wuc --init-groups "$@"
fi
exec "$@"
