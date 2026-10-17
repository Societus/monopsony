Name: monopsony-client
Version: 1.0.0-rc2
Release: 1%{?dist}
Summary: Monopsony client daemon — local HTTP proxy and package manager integration
License: MIT
URL: https://base.societus.club/societus/monopsony
Source0: monopsony-client-%{version}
BuildArch: %{_arch}
Conflicts: monopsony-server

%description
Monopsony is a centralized package caching proxy system. The client runs a
local HTTP proxy (127.0.0.1:3129) that redirects package manager traffic
(apt, pacman, dnf, apk, xbps, emerge, zypper, winget, choco, scoop, brew,
npm, pip, cargo) through a central Monopsony server cache. It also handles
server pairing (mTLS), shadow package management, and distribution mirror
configuration.

This package installs the client binary only. The server (monopsony-server)
is packaged separately and requires PostgreSQL.

After installation:
  1. Pair with a server: monopsony-client pair --server <host> --code <code>
  2. Start the daemon: systemctl enable --now monopsony-client
  3. Configure package managers: monopsony-client configure --server <host>

%prep
# Source is a prebuilt binary placeholder; %install copies it into place.

%build
# Nothing to build — binary shipped prebuilt.

%install
install -d %{buildroot}%{_bindir}
install -m 0755 %{SOURCE0} %{buildroot}%{_bindir}/monopsony-client

install -d %{buildroot}%{_sysconfdir}/monopsony
install -m 0644 /dev/null %{buildroot}%{_sysconfdir}/monopsony/client.toml.example

install -d %{buildroot}%{_unitdir}
install -m 0644 /dev/null %{buildroot}%{_unitdir}/monopsony-client.service

install -d %{buildroot}%{_tmpfilesdir}
install -m 0644 /dev/null %{buildroot}%{_tmpfilesdir}/monopsony-client.conf

install -d %{buildroot}%{_sysusersdir}
install -m 0644 /dev/null %{buildroot}%{_sysusersdir}/monopsony-client.conf

install -d %{buildroot}%{_licensedir}/monopsony-client
install -m 0644 /dev/null %{buildroot}%{_licensedir}/monopsony-client/LICENSE

%clean
rm -rf %{buildroot}

%files
%{_bindir}/monopsony-client
%config(noreplace) %{_sysconfdir}/monopsony/client.toml.example
%{_unitdir}/monopsony-client.service
%{_tmpfilesdir}/monopsony-client.conf
%{_sysusersdir}/monopsony-client.conf
%license %{_licensedir}/monopsony-client/LICENSE

%changelog
* Thu Oct 17 2026 Monopsony Project <packaging@societus.club> - 1.0.0-rc2-1
- Initial RPM package for monopsony-client 1.0.0-rc2
