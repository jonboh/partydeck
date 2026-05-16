{
  description = "PartyDeck - local multiplayer game launcher";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = nixpkgs.legacyPackages.${system};

      # Pre-fetch the bundled runtime deps that build.rs copies into the binary dir.
      # These are fetched as fixed-output derivations and extracted before the
      # Rust build runs so that build.rs can copy them without network access.
      gbeForkLinux = pkgs.stdenvNoCC.mkDerivation {
        name = "gbe-fork-linux";
        src = pkgs.fetchurl {
          url = "https://github.com/Detanup01/gbe_fork/releases/download/release-2026_03_10/emu-linux-release.tar.bz2";
          hash = "sha256-AyUAyhALcv0tqpTuwyY898b+Y0h2I/nijXDu5BpYuwE=";
        };
        nativeBuildInputs = [pkgs.gnutar pkgs.bzip2];
        buildCommand = ''
          mkdir -p $out
          tar -xjf $src -C $out
          # build.rs renames the extracted "release" dir to "gbe-linux"
          if [ -d "$out/release" ]; then
            mv $out/release $out/gbe-linux
          fi
        '';
      };

      gbeForkWin = pkgs.stdenvNoCC.mkDerivation {
        name = "gbe-fork-win";
        src = pkgs.fetchurl {
          url = "https://github.com/Detanup01/gbe_fork/releases/download/release-2026_03_10/emu-win-release.7z";
          hash = "sha256-D2ekISqk5qcfhIeaOgD2dcsqjEPhPjjgsnq1yeal5l8=";
        };
        nativeBuildInputs = [pkgs.p7zip];
        buildCommand = ''
          mkdir -p $out
          7z x $src -o$out
          # build.rs renames the extracted "release" dir to "gbe-win"
          if [ -d "$out/release" ]; then
            mv $out/release $out/gbe-win
          fi
        '';
      };

      umuLauncher = pkgs.stdenvNoCC.mkDerivation {
        name = "umu-launcher";
        src = pkgs.fetchurl {
          url = "https://github.com/Open-Wine-Components/umu-launcher/releases/download/1.3.0/umu-launcher-1.3.0-zipapp.tar";
          hash = "sha256-NlAt52bzzFSf+FGWoE+1r9tOsqcsAj8i/SWJXfkf2i8=";
        };
        nativeBuildInputs = [pkgs.gnutar];
        buildCommand = ''
          mkdir -p $out
          tar -xf $src -C $out
          if [ ! -d "$out/umu" ]; then
            echo "ERROR: expected top-level 'umu/' directory not found in archive"
            ls "$out"
            exit 1
          fi
        '';
      };

      # Native libraries required at runtime by egui/eframe (GL, wayland, X11)
      runtimeLibs = with pkgs; [
        libGL
        libxkbcommon
        wayland
        xorg.libX11
        xorg.libXcursor
        xorg.libXrandr
        xorg.libXi
        dbus
      ];

      partydeck = pkgs.rustPlatform.buildRustPackage {
        pname = "partydeck";
        version = "0.8.5";

        src = ./.;

        cargoLock.lockFile = ./Cargo.lock;

        nativeBuildInputs = with pkgs; [
          pkg-config
          makeWrapper
        ];

        buildInputs = with pkgs; [
          openssl
          dbus
          libxkbcommon
          wayland
          xorg.libX11
          xorg.libXcursor
          xorg.libXrandr
          xorg.libXi
          libGL
        ];

        # Place pre-fetched archives where build.rs expects them so it can
        # copy them into the output directory without any network access.
        preBuild = ''
          mkdir -p deps/releases
          cp -r ${gbeForkLinux}/gbe-linux deps/releases/gbe-linux
          cp -r ${gbeForkWin}/gbe-win     deps/releases/gbe-win
          cp -r ${umuLauncher}/umu         deps/releases/umu
          chmod -R u+w deps/releases
        '';

        # Wrap the binary so egui can find the GL/Wayland/X11 libs at runtime,
        # and install the resource files that build.rs placed next to the binary.
        postInstall = ''
          targetReleaseDir="target/${pkgs.stdenv.hostPlatform.config}/release"

          # Install bundled resources (KWin scripts, Goldberg steam emulator libs)
          if [ -d "$targetReleaseDir/res" ]; then
            cp -r "$targetReleaseDir/res" "$out/bin/res"
          fi

          # Install umu-run launcher if present
          if [ -f "$targetReleaseDir/bin/umu-run" ]; then
            install -Dm755 "$targetReleaseDir/bin/umu-run" "$out/bin/umu-run"
          fi

          wrapProgram $out/bin/partydeck \
            --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath runtimeLibs}"
        '';

        meta = with pkgs.lib; {
          description = "Local multiplayer game launcher with split-screen support";
          homepage = "https://github.com/xDavidLeon/partydeck";
          license = licenses.gpl3Only;
          mainProgram = "partydeck";
          platforms = platforms.linux;
        };
      };
    in {
      packages = {
        inherit partydeck;
        default = partydeck;
      };

      devShells.default = pkgs.mkShell {
        name = "partydeck-devshell";
        packages = with pkgs; [
          rustup
          openssl
          pkg-config
          cargo-deny
          cargo-edit
          cargo-expand
          cargo-nextest
          rust-analyzer
          bacon
          # native libs for egui build/link
          libxkbcommon
          wayland
          xorg.libX11
          xorg.libXcursor
          xorg.libXrandr
          xorg.libXi
          libGL
          dbus
        ];
        env.LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;
      };
    });
}
