#!/bin/bash
set -e

# Create swap if SWAP_SIZE > 0 and no swap exists
if [ "${SWAP_SIZE:-1024}" -gt 0 ] && [ "$(swapon --show | wc -l)" -le 1 ]; then
    echo "Creating ${SWAP_SIZE}MB swapfile..."
    dd if=/dev/zero of=/tmp/swapfile bs=1M count="${SWAP_SIZE}" 2>/dev/null
    chmod 600 /tmp/swapfile
    mkswap /tmp/swapfile >/dev/null
    swapon /tmp/swapfile || true
    echo "Swap active."
fi

exec "$@"
