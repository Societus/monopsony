SYNTHETIC fixtures — signed with throwaway test keys, NOT real distro trust anchors.

Regenerate:
  cargo test -p monopsony-mirror generate_fixtures -- --ignored

debian/: clearsigned InRelease + detached Release.gpg over Release (apt-secure(8) layout).
alpine/: two-member APKINDEX.tar.gz (.SIGN.RSA.<key>.rsa.pub over member 2 compressed bytes, RSA-PKCS1v1.5-SHA1 — the live Alpine scheme).
Generated 2026-10-31T18:14:05.167129917+00:00.
