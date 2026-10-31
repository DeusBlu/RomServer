#!/bin/sh
# Downloads the pinned htmx build into assets/ (it is embedded in the binary, but not committed).
# Run once after cloning; the Dockerfile runs it too. Bump both values together.
set -eu

HTMX_VERSION="2.0.4"
HTMX_SHA256="e209dda5c8235479f3166defc7750e1dbcd5a5c1808b7792fc2e6733768fb447"

destination="$(dirname "$0")/../assets/htmx.min.js"

if [ -f "$destination" ] && echo "$HTMX_SHA256  $destination" | sha256sum -c - >/dev/null 2>&1; then
    echo "htmx $HTMX_VERSION already present"
    exit 0
fi

temporary="$destination.download"
curl -fsSL "https://unpkg.com/htmx.org@$HTMX_VERSION/dist/htmx.min.js" -o "$temporary"
if ! echo "$HTMX_SHA256  $temporary" | sha256sum -c - >/dev/null 2>&1; then
    rm -f "$temporary"
    echo "htmx $HTMX_VERSION checksum mismatch; refusing to use the download" >&2
    exit 1
fi
mv "$temporary" "$destination"
echo "htmx $HTMX_VERSION downloaded"
