Name:           athen
Version:        0.1.10
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
BuildRequires:  librsvg2-devel
BuildRequires:  desktop-file-utils
BuildRequires:  libxkbcommon-devel
BuildRequires:  cmake
BuildRequires:  nasm
BuildRequires:  curl
BuildRequires:  tar

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
# Fetch the nushell sidecar that Tauri's build script expects at
# crates/athen-app/binaries/nu-<triple>. Mirrors what CI does before cargo build.
%ifarch x86_64
TARGET_TRIPLE=x86_64-unknown-linux-gnu bash scripts/fetch-nushell.sh
%endif
%ifarch aarch64
TARGET_TRIPLE=aarch64-unknown-linux-gnu bash scripts/fetch-nushell.sh
%endif

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
* Tue May 12 2026 Alejandro Garcia <contact@alejandrogarcia.blog> - 0.1.10-1
- Release 0.1.10

* Mon May 11 2026 Alejandro Garcia <contact@alejandrogarcia.blog> - 0.1.9-1
- Release 0.1.9

* Mon May 11 2026 Alejandro Garcia <contact@alejandrogarcia.blog> - 0.1.8-1
- Release 0.1.8

* Mon May 11 2026 Alejandro Garcia <contact@alejandrogarcia.blog> - 0.1.7-1
- Release 0.1.7

* Sun May 10 2026 Alejandro Garcia <contact@alejandrogarcia.blog> - 0.1.6-1
- Release 0.1.6

* Sat May 09 2026 Alejandro Garcia <contact@alejandrogarcia.blog> - 0.1.5-1
- Release 0.1.5

* Fri May 08 2026 Alejandro Garcia <contact@alejandrogarcia.blog> - 0.1.4-1
- Release 0.1.4

* Fri May 08 2026 Alejandro Garcia <contact@alejandrogarcia.blog> - 0.1.3-1
- Release 0.1.3

* Thu May 07 2026 Alejandro Garcia <contact@alejandrogarcia.blog> - 0.1.2-1
- Load models.toml at startup even when config.toml is absent (fixes
  empty-providers router build for users who only configured an LLM key).
- Suppress Windows console window flashes when spawning subprocesses.

* Wed May 06 2026 Alejandro Garcia <contact@alejandrogarcia.blog> - 0.1.1-1
- Initial COPR build
