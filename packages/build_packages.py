#!/usr/bin/env python3
"""Build .deb, .rpm, and Arch .pkg.tar.zst packages for monopsony-client.

Usage: python3 packages/build_packages.py
"""
import os
import sys
import stat
import shutil
import subprocess
import tarfile
import io
import struct
import gzip

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------
SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
PROJECT_ROOT = os.path.dirname(SCRIPT_DIR)
STAGING = os.path.join(SCRIPT_DIR, "staging")
BUILD = os.path.join(SCRIPT_DIR, "build")
VERSION = "1.0.0-rc2"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "x86_64"
DEB_ARCH = "arm64" if ARCH == "aarch64" else "amd64"

os.makedirs(BUILD, exist_ok=True)


# ---------------------------------------------------------------------------
# .deb package
# ---------------------------------------------------------------------------
def build_deb():
    """Build a Debian .deb package using system `ar`.

    A .deb is an ar archive containing:
      deb-binary   -> "2.0"
      control.tar.gz -> control file
      data.tar.xz  -> the actual files
    """
    deb_path = os.path.join(BUILD, f"monopsony-client_{VERSION}_{DEB_ARCH}.deb")
    tmpdir = os.path.join(BUILD, "deb_tmp")
    if os.path.exists(tmpdir):
        shutil.rmtree(tmpdir)
    os.makedirs(tmpdir)

    # --- deb-binary ---
    with open(os.path.join(tmpdir, "deb-binary"), "wb") as f:
        f.write(b"2.0\n")

    # --- control.tar.gz ---
    control_text = (
        "Package: monopsony-client\n"
        "Version: 1.0.0-rc2\n"
        "Section: net\n"
        "Priority: optional\n"
        f"Architecture: {DEB_ARCH}\n"
        "Maintainer: Monopsony Project <packaging@societus.club>\n"
        "Depends: adduser\n"
        "Conflicts: monopsony-server\n"
        "Description: Monopsony client daemon - local HTTP proxy and package manager integration\n"
        " Monopsony is a centralized package caching proxy system. The client runs a\n"
        " local HTTP proxy (127.0.0.1:3129) that redirects package manager traffic\n"
        " (apt, pacman, dnf, apk, xbps, emerge, zypper, winget, choco, scoop, brew,\n"
        " npm, pip, cargo) through a central Monopsony server cache. It also handles\n"
        " server pairing (mTLS), shadow package management, and distribution mirror\n"
        " configuration.\n"
        "\n"
        " This package installs the client binary only. The server (monopsony-server)\n"
        " is packaged separately and requires PostgreSQL.\n"
        "\n"
        " After installation:\n"
        "   1. Pair with a server: monopsony-client pair --server <host> --code <code>\n"
        "   2. Start the daemon: systemctl enable --now monopsony-client\n"
        "   3. Configure package managers: monopsony-client configure --server <host>\n"
    )
    control_dir = os.path.join(tmpdir, "control")
    os.makedirs(control_dir)
    with open(os.path.join(control_dir, "control"), "w") as f:
        f.write(control_text)

    with tarfile.open(os.path.join(tmpdir, "control.tar.gz"), "w:gz") as tf:
        tf.add(os.path.join(control_dir, "control"), arcname="control")

    # --- data.tar.xz ---
    with tarfile.open(os.path.join(tmpdir, "data.tar.xz"), "w:xz") as tf:
        for dirpath, dirnames, filenames in os.walk(STAGING):
            for name in sorted(dirnames + filenames):
                full = os.path.join(dirpath, name)
                rel = os.path.relpath(full, STAGING)
                arcname = "/" + rel
                tf.add(full, arcname=arcname, recursive=False)

    # --- assemble .deb via `ar` ---
    subprocess.run(
        ["ar", "rc", deb_path,
         os.path.join(tmpdir, "deb-binary"),
         os.path.join(tmpdir, "control.tar.gz"),
         os.path.join(tmpdir, "data.tar.xz")],
        check=True,
    )

    shutil.rmtree(tmpdir)
    print(f"[deb] {deb_path} ({os.path.getsize(deb_path)} bytes)")
    return deb_path


# ---------------------------------------------------------------------------
# .rpm package (manual construction — no rpmbuild available)
# ---------------------------------------------------------------------------
def build_rpm():
    """Build an RPM package manually.

    RPM v3 format:
      Lead: 128 bytes (magic \xed\xab\xee\xdb, version, release, etc.)
      Header: magic "8RPM" + index entries + data store
      Payload: gzip-compressed cpio archive (newc format)
    """
    rpm_path = os.path.join(BUILD, f"monopsony-client-{VERSION}-1.{ARCH}.rpm")

    # --- Build cpio archive (newc format) ---
    cpio_buf = io.BytesIO()

    def cpio_newc_header(name, mode, size, mtime=0):
        header = (
            f"{0o070701:06o}"
            f"{0:06o}"
            f"{0:06o}"
            f"{mode & 0o7777:06o}"
            f"{0:06o}"
            f"{0:06o}"
            f"{1:06o}"
            f"{mtime:011o}"
            f"{size:011o}"
            f"{0:06o}"
            f"{0:06o}"
            f"{0:06o}"
            f"{0:06o}"
            f"{len(name) + 1:06o}"
            f"{0:06o}"
        )
        return header.encode()

    def cpio_trailer():
        name = "TRAILER!!!\0"
        hdr = cpio_newc_header(name, 0, 0)
        hdr += name.encode()
        pad = (-len(hdr)) % 2
        hdr += b"\0" * pad
        return hdr

    entries = []
    for dirpath, dirnames, filenames in os.walk(STAGING):
        for name in dirnames + filenames:
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, STAGING)
            arcname = "./" + rel
            entries.append((full, arcname))

    entries.sort(key=lambda e: e[1])

    for full, arcname in entries:
        st = os.stat(full)
        mode = st.st_mode
        if os.path.isdir(full):
            size = 0
            hdr = cpio_newc_header(arcname, mode, size)
            cpio_buf.write(hdr)
            cpio_buf.write((arcname + "\0").encode())
            pad = (-len(hdr) - len(arcname) - 1) % 2
            cpio_buf.write(b"\0" * pad)
        else:
            with open(full, "rb") as f:
                data = f.read()
            size = len(data)
            hdr = cpio_newc_header(arcname, mode, size)
            cpio_buf.write(hdr)
            cpio_buf.write((arcname + "\0").encode())
            pad = (-len(hdr) - len(arcname) - 1) % 2
            cpio_buf.write(b"\0" * pad)
            cpio_buf.write(data)
            pad = (-size) % 2
            cpio_buf.write(b"\0" * pad)

    cpio_buf.write(cpio_trailer())
    cpio_data = cpio_buf.getvalue()
    gz_cpio = gzip.compress(cpio_data, mtime=0)

    # --- Build header ---
    RPMTAG_NAME = 1000
    RPMTAG_VERSION = 1001
    RPMTAG_RELEASE = 1002
    RPMTAG_SUMMARY = 1004
    RPMTAG_DESCRIPTION = 1005
    RPMTAG_BUILDTIME = 1006
    RPMTAG_SIZE = 1009
    RPMTAG_OS = 1010
    RPMTAG_ARCH = 1011
    RPMTAG_LICENSE = 1014
    RPMTAG_GROUP = 1015
    RPMTAG_URL = 1020
    RPMTAG_PAYLOADFORMAT = 1126
    RPMTAG_PAYLOADCOMPRESSOR = 1125
    RPMTAG_FILES = 1026
    RPMTAG_FILEMODES = 1032
    RPMTAG_FILEUIDS = 1035
    RPMTAG_FILEGIDS = 1036
    RPMTAG_FILESIZES = 1038
    RPMTAG_FILEMTIMES = 1041
    RPMTAG_FILEFLAGS = 1080
    RPMTAG_CONFLICTNAME = 1064
    RPMTAG_CONFLICTVERSION = 1065
    RPMTAG_CONFLICTFLAGS = 1091

    data_store = bytearray()
    index_entries = []

    def add_string(s):
        if isinstance(s, bytes):
            b = s + b"\0"
        else:
            b = s.encode("utf-8") + b"\0"
        off = len(data_store)
        data_store.extend(b)
        return off

    def add_entry(tag, itype, count, offset):
        index_entries.append((tag, itype, offset, count))

    def add_int_array(values):
        off = len(data_store)
        for v in values:
            data_store.extend(struct.pack(">i", v))
        return off

    mtime = 0

    file_names = []
    file_modes = []
    file_uids = []
    file_gids = []
    file_sizes = []
    file_mtimes = []
    file_flags = []

    for full, arcname in entries:
        st = os.stat(full)
        file_names.append(add_string(arcname))
        file_modes.append(st.st_mode & 0o7777)
        file_uids.append(st.st_uid)
        file_gids.append(st.st_gid)
        file_sizes.append(st.st_size if not os.path.isdir(full) else 0)
        file_mtimes.append(int(st.st_mtime))
        file_flags.append(0)

    add_entry(RPMTAG_NAME, 8, 1, add_string("monopsony-client"))
    add_entry(RPMTAG_VERSION, 8, 1, add_string(VERSION))
    add_entry(RPMTAG_RELEASE, 8, 1, add_string("1"))
    add_entry(RPMTAG_SUMMARY, 8, 1, add_string("Monopsony client daemon - local HTTP proxy"))
    add_entry(RPMTAG_DESCRIPTION, 8, 1,
              add_string("Monopsony client - local HTTP proxy and package manager integration for the Monopsony caching proxy system"))
    add_entry(RPMTAG_BUILDTIME, 7, 1, add_string(struct.pack(">i", mtime)))
    bin_size = os.path.getsize(os.path.join(STAGING, "usr/bin/monopsony-client"))
    add_entry(RPMTAG_SIZE, 7, 1, add_string(struct.pack(">i", bin_size)))
    add_entry(RPMTAG_OS, 8, 1, add_string("linux"))
    add_entry(RPMTAG_ARCH, 8, 1, add_string(ARCH))
    add_entry(RPMTAG_LICENSE, 8, 1, add_string("MIT"))
    add_entry(RPMTAG_GROUP, 8, 1, add_string("System Environment/Daemons"))
    add_entry(RPMTAG_URL, 8, 1, add_string("https://base.societus.club/societus/monopsony"))
    add_entry(RPMTAG_PAYLOADFORMAT, 8, 1, add_string("cpio"))
    add_entry(RPMTAG_PAYLOADCOMPRESSOR, 8, 1, add_string("gzip"))

    add_entry(RPMTAG_FILES, 8, len(file_names), add_int_array(file_names))
    add_entry(RPMTAG_FILEMODES, 7, len(file_modes), add_int_array(file_modes))
    add_entry(RPMTAG_FILEUIDS, 7, len(file_uids), add_int_array(file_uids))
    add_entry(RPMTAG_FILEGIDS, 7, len(file_gids), add_int_array(file_gids))
    add_entry(RPMTAG_FILESIZES, 7, len(file_sizes), add_int_array(file_sizes))
    add_entry(RPMTAG_FILEMTIMES, 7, len(file_mtimes), add_int_array(file_mtimes))
    add_entry(RPMTAG_FILEFLAGS, 7, len(file_flags), add_int_array(file_flags))

    add_entry(RPMTAG_CONFLICTNAME, 8, 1, add_string("monopsony-server"))
    add_entry(RPMTAG_CONFLICTVERSION, 8, 1, add_string(""))
    add_entry(RPMTAG_CONFLICTFLAGS, 7, 1, add_int_array([0]))

    header_data = bytearray()
    header_data.extend(b"8RPM")
    header_data.extend(struct.pack(">i", len(index_entries)))
    data_store_offset = 16 + 16 * len(index_entries)
    data_store_offset = (data_store_offset + 7) & ~7
    header_data.extend(struct.pack(">i", data_store_offset))

    for tag, itype, offset, count in index_entries:
        header_data.extend(struct.pack(">iiii", tag, itype, offset, count))

    while len(header_data) < data_store_offset:
        header_data.extend(b"\0")

    header_data.extend(data_store)
    header_bytes = bytes(header_data)

    # --- Lead (128 bytes) ---
    lead = bytearray(128)
    lead[0:4] = b"\xed\xab\xee\xdb"
    lead[4:6] = struct.pack(">H", 3)
    lead[6:8] = struct.pack(">H", 100)

    with open(rpm_path, "wb") as f:
        f.write(lead)
        f.write(header_bytes)
        f.write(gz_cpio)

    print(f"[rpm] {rpm_path} ({os.path.getsize(rpm_path)} bytes)")
    return rpm_path


# ---------------------------------------------------------------------------
# Arch .pkg.tar.zst via makepkg
# ---------------------------------------------------------------------------
def build_arch():
    """Build Arch Linux package using makepkg."""
    arch_dir = os.path.join(SCRIPT_DIR, "arch")
    pkgbuild = os.path.join(arch_dir, "PKGBUILD")

    src_binary = os.path.join(arch_dir, f"monopsony-client-{VERSION.replace('-', '_')}")
    shutil.copy2(os.path.join(STAGING, "usr/bin/monopsony-client"), src_binary)
    os.chmod(src_binary, 0o755)

    result = subprocess.run(
        ["makepkg", "-sf", "--nodeps", "--noconfirm"],
        cwd=arch_dir,
        capture_output=True,
        text=True,
    )
    print(f"[arch] makepkg exit code: {result.returncode}")
    if result.stdout:
        print(result.stdout[-2000:])
    if result.stderr:
        print(result.stderr[-2000:])

    for fn in os.listdir(arch_dir):
        if fn.endswith(".pkg.tar.zst"):
            pkg_path = os.path.join(arch_dir, fn)
            print(f"[arch] {pkg_path} ({os.path.getsize(pkg_path)} bytes)")
            return pkg_path
    print("[arch] No .pkg.tar.zst found!")
    return None


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
if __name__ == "__main__":
    results = []
    results.append(build_deb())
    results.append(build_rpm())
    results.append(build_arch())
    print("\n=== Built packages ===")
    for r in results:
        if r:
            print(f"  {r}")
    print("\nNote: .dmg (macOS) cannot be built on Linux — requires macOS hdiutil.")
    print("Note: aarch64 builds need a cross-compilation toolchain (musl-gcc, not installed).")
