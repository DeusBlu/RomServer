#!/bin/sh
# RomServer runs as root, because Unraid's share settings (/boot/config/shares) are readable by
# root only, and hands everything it creates to PUID:PGID (src/ownership.rs). UMASK keeps what it
# creates writable over SMB like the rest of the share.
set -eu

umask "${UMASK:-000}"
# /config is RomServer's own (database, settings, undo); bring anything older in line.
if [ -n "${PUID:-}" ] && [ -n "${PGID:-}" ] && [ -d /config ]; then
    chown -R "$PUID:$PGID" /config
fi
exec romserver "$@"
