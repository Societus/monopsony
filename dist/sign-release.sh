#!/usr/bin/env bash
# Release signing script for Monopsony
# Usage: ./sign-release.sh <version> <binary-dir>
# Example: ./sign-release.sh v0.1.0-rc1 ./target/release
#
# Prerequisites:
#   1. Generate a release signing key (one-time):
#      gpg --batch --gen-key <<EOF
#      %no-protection
#      Key-Type: ed25519
#      Key-Usage: sign
#      Name-Real: Monopsony Release Signing
#      Name-Email: release@monopsony.societus.club
#      Expire-Date: 0
#      EOF
#   2. Export the public key:
#      gpg --armor --export release@monopsony.societus.club > monopsony-release-key.asc
#
# This script:
#   - Generates SHA256 checksums for all binaries
#   - Signs the checksums file with GPG
#   - Creates detached signatures for each binary

set -euo pipefail

VERSION="${1:?Usage: sign-release.sh <version> <binary-dir>}"
BINDIR="${2:?Usage: sign-release.sh <version> <binary-dir>}"
SIGNING_KEY="release@monopsony.societus.club"
OUTDIR="./release-artifacts/${VERSION}"

mkdir -p "${OUTDIR}"

echo "=== Monopsony Release Signing ==="
echo "Version: ${VERSION}"
echo "Binary dir: ${BINDIR}"
echo "Output dir: ${OUTDIR}"

# Copy binaries
for bin in monopsony-server monopsony-client monopsony-tracker; do
    if [[ -f "${BINDIR}/${bin}" ]]; then
        cp "${BINDIR}/${bin}" "${OUTDIR}/${bin}"
        echo "Copied: ${bin}"
    else
        echo "WARNING: ${bin} not found in ${BINDIR}"
    fi
done

# Generate SHA256 checksums
echo "Generating SHA256 checksums..."
(cd "${OUTDIR}" && sha256sum monopsony-* > SHA256SUMS 2>/dev/null || true)
echo "SHA256SUMS:"
cat "${OUTDIR}/SHA256SUMS"

# Sign the checksums file
echo "Signing checksums with GPG..."
GPG_OPTS="--default-key ${SIGNING_KEY} --batch --yes --pinentry-mode loopback --passphrase ''"
if gpg --list-secret-keys "${SIGNING_KEY}" &>/dev/null; then
    gpg ${GPG_OPTS} --detach-sign --armor "${OUTDIR}/SHA256SUMS"
    gpg ${GPG_OPTS} --clearsign "${OUTDIR}/SHA256SUMS"
    echo "Signed: SHA256SUMS.asc, SHA256SUMS.txt"

    # Fingerprint for documentation
    echo ""
    echo "Public key fingerprint:"
    gpg --fingerprint "${SIGNING_KEY}" | head -4
else
    echo "ERROR: No GPG key found for ${SIGNING_KEY}"
    echo "Generate one with:"
    echo "  gpg --batch --gen-key <<EOF"
    echo "  %no-protection"
    echo "  Key-Type: ed25519"
    echo "  Key-Usage: sign"
    echo "  Name-Real: Monopsony Release Signing"
    echo "  Name-Email: ${SIGNING_KEY}"
    echo "  Expire-Date: 0"
    echo "  EOF"
    exit 1
fi

# Create detached signatures for each binary
echo "Signing individual binaries..."
for bin in "${OUTDIR}"/monopsony-*; do
    [[ -f "${bin}" ]] || continue
    [[ "${bin}" == *.asc ]] && continue
    gpg ${GPG_OPTS} --detach-sign --armor "${bin}"
    echo "Signed: $(basename "${bin}").asc"
done

echo ""
echo "=== Release artifacts in ${OUTDIR}/ ==="
ls -la "${OUTDIR}/"
echo ""
echo "Upload these files to the GitHub/Forgejo release page."
echo "Include monopsony-release-key.asc in the repo for verification."
