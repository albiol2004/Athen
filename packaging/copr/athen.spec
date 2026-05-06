Name:           athen
Version:        0.1.1
Release:        1%{?dist}
Summary:        Universal proactive AI agent (Tauri 2 desktop app)

License:        MIT
URL:            https://github.com/albiol2004/Athen
Source0:        %{url}/archive/refs/tags/v%{version}.tar.gz#/%{name}-%{version}.tar.gz

BuildRequires:  rust >= 1.77
BuildRequires:  cargo
BuildRequires:  pkgconf-pkg-config
BuildRequires:  webkit2gtk4.1-devel
BuildRequires:  gtk3-devel
BuildRequires:  libsoup3-devel
BuildRequires:  libappindicator-gtk3-devel
BuildRequires:  desktop-file-utils
BuildRequires:  libxkbcommon-devel

Requires:       webkit2gtk4.1
Requires:       gtk3
Requires:       libsoup3
Requires:       libappindicator-gtk3
Requires:       hicolor-icon-theme

Recommends:     podman
Suggests:       nushell

ExclusiveArch:  x86_64 aarch64

%description
Athen is a universal, proactive AI agent that monitors emails, calendar,
messages, and direct input ("senses"), evaluates what needs doing, and
executes tasks autonomously with a dynamic risk system.

%global debug_package %{nil}

%prep
%autosetup -n Athen-%{version}

%build
# Build only the desktop app crate; the workspace contains library crates and
# CLI tooling that are not part of the user-facing binary.
cargo build --release --offline -p athen-app || cargo build --release -p athen-app

%install
install -Dm755 target/release/athen-app %{buildroot}%{_bindir}/athen-app
install -Dm644 LICENSE %{buildroot}%{_datadir}/licenses/%{name}/LICENSE

# Desktop file + icons (best-effort; the upstream paths track the Tauri bundle layout)
for size in 32 64 128 256 512; do
    src="crates/athen-app/icons/${size}x${size}.png"
    [ -f "$src" ] || continue
    install -Dm644 "$src" \
        "%{buildroot}%{_datadir}/icons/hicolor/${size}x${size}/apps/athen-app.png"
done

cat > %{buildroot}/Athen.desktop <<EOF
[Desktop Entry]
Name=Athen
Comment=Universal proactive AI agent
Exec=athen-app
Icon=athen-app
Type=Application
Categories=Utility;
Terminal=false
StartupWMClass=athen-app
EOF
desktop-file-install \
    --dir=%{buildroot}%{_datadir}/applications \
    %{buildroot}/Athen.desktop
rm %{buildroot}/Athen.desktop

%files
%license LICENSE
%{_bindir}/athen-app
%{_datadir}/applications/Athen.desktop
%{_datadir}/icons/hicolor/*/apps/athen-app.png

%changelog
* Wed May 06 2026 Alejandro Garcia <albiol2004@gmail.com> - 0.1.1-1
- Initial COPR build
