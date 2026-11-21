# Built offline from the vendored crates (`make srpm` writes both tarballs).
# `make archive` (also Packit's create-archive action) sets the two globals
# below, bree_version from Cargo.toml and bree_commit from git, since the
# tarball has no .git for build.rs to read; `make srpm` builds the SRPM from
# that copy in rpm/. The values here only keep the file parseable (Packit reads
# it before it runs the action) and show as "unfilled" in a build that skipped
# `make archive`. Defines given to `rpmbuild -bs` do not reach a --rebuild.
%global bree_version 0.0.0.unfilled
%global bree_commit unfilled
%global debug_package %{nil}

Name:           bree
Version:        %{bree_version}
Release:        1%{?dist}
Summary:        Run coding agents in containers from profiles
License:        MIT
Source0:        %{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.gz

BuildRequires:  cargo
BuildRequires:  gcc

%description
bree runs a coding agent (Claude Code, pi or opencode) in a rootless podman
container set up from a profile, and renders the agent's configuration from
named building blocks.

%prep
%autosetup -n %{name}-%{version} -a 1
mkdir -p .cargo
cat > .cargo/config.toml <<'CFG'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
CFG

%build
export BREE_COMMIT=%{bree_commit}
cargo build --release --locked --offline

%install
install -Dpm 0755 target/release/bree %{buildroot}%{_bindir}/bree

%check
target/release/bree --version

%files
%license LICENSE
%doc README.md
%{_bindir}/bree

%changelog
* Mon Nov 16 2026 Tomas Tomecek <tomas@tomecek.net> - 0.1.0-1
- Initial package
