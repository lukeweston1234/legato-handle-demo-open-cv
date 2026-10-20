{
  inputs.nixpkgs.url = "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz";

  outputs = { self, nixpkgs }:
    let
      forEachSystem = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
    in
    {
      devShells = forEachSystem (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          python = pkgs.python3;

          # Runtime libs the manylinux wheels (mediapipe, opencv) dlopen.
          wheelLibs = with pkgs; [
            stdenv.cc.cc.lib
            zlib
            glib
            libGL
            libx11
            libxext
            libxrender
            libxcb
            libsm
            libice
            fontconfig
            freetype
            # The viewer's winit + glow, which dlopen these too.
            wayland
            libxkbcommon
            libxcursor
            libxrandr
            libxi
          ];
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [ rustc cargo clippy rustfmt rust-analyzer uv python ffmpeg ];
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux (with pkgs; [ alsa-lib jack2 ]);
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";

            UV_PYTHON = "${python}/bin/python";
            UV_PYTHON_DOWNLOADS = "never";
            # opencv-python wheels only bundle the Qt xcb plugin (runs via XWayland).
            QT_QPA_PLATFORM = "xcb";

            shellHook = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              # PipeWire's libjack first, so the JACK host runs on PipeWire without `pw-jack`.
              export LD_LIBRARY_PATH="${pkgs.pipewire.jack}/lib:${pkgs.lib.makeLibraryPath wheelLibs}:/run/opengl-driver/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
            '';
          };
        });
    };
}
