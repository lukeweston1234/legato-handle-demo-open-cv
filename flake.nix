{
  inputs = {
    nixpkgs.url = "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz";

    pyproject-nix.url= "github:pyproject-nix/pyproject.nix";
    pyproject-nix.inputs.nixpkgs.follows = "nixpkgs";
    uv2nix.url = "github:pyproject-nix/uv2nix";
    uv2nix.inputs.nixpkgs.follows = "nixpkgs";
    pyproject-build-systems.url = "github:pyproject-nix/build-system-pkgs";
    pyproject-build-systems.inputs.nixpkgs.follows = "nixpkgs";

    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, pyproject-nix, uv2nix, pyproject-build-systems, crane }:
    let
      forEachSystem = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];

      # Runtime libs the manylinux wheels (mediapipe, opencv) dlopen.
      wheelLibsPkgs = pkgs: with pkgs; [
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
    in
    {
      packages."x86_64-linux" =
        let
          pkgs = nixpkgs.legacyPackages."x86_64-linux";
          inherit (pkgs) lib;

          handModel = pkgs.fetchurl {
            url = "https://storage.googleapis.com/mediapipe-models/hand_landmarker/hand_landmarker/float16/latest/hand_landmarker.task";
            hash= "sha256-+8KjAIDDxVcJO13fwzRpgTLrNBBEzO4yLM+LzzYHzeE=";
          };

          # for python
          python = pkgs.python3;
          pythonBase = pkgs.callPackage pyproject-nix.build.packages {
            inherit python;
          };
          workspace = uv2nix.lib.workspace.loadWorkspace { workspaceRoot = ./.; };
          overlay = workspace.mkPyprojectOverlay {
            sourcePreference = "wheel";
          };
          pythonSet = pythonBase.overrideScope (
            lib.composeManyExtensions [
              pyproject-build-systems.overlays.wheel
              overlay
              (final: prev: {
                legato-hand-tracking = prev.legato-hand-tracking.overrideAttrs (old: {
                  postInstall = (old.postInstall or "") + ''
                    mkdir -p $out/${final.python.sitePackages}/legato_hand_tracking/models
                    cp ${handModel} $out/${final.python.sitePackages}/legato_hand_tracking/models/hand_landmarker.task
                  '';
                });
              })
            ]
          );
          inherit (pkgs.callPackages pyproject-nix.build.util { }) mkApplication;
          legato-hand-tracking = mkApplication {
            venv = pythonSet.mkVirtualEnv "legato-hand-tracking-env" { legato-hand-tracking = []; };
            package = pythonSet.legato-hand-tracking;
          };
          # for rust
          craneLib = crane.mkLib pkgs;
          commonArgs = {
            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [
                (craneLib.fileset.commonCargoSources ./.)
                ./marionette.legato
              ];
            };
            strictDeps = true;
            buildInputs = with pkgs; [
              alsa-lib
              jack2
            ];
            nativeBuildInputs = with pkgs; [
              pkg-config
              ffmpeg
            ];
          };
          wheelLibs = wheelLibsPkgs pkgs;
          cargoArtifacts = craneLib.buildDepsOnly (commonArgs // {
            pname = "mycrate-deps";
          });
          legato-hand-tracking-sampler = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
              nativeBuildInputs = (commonArgs.nativeBuildInputs or [ ]) ++ [ pkgs.makeWrapper ];

              postPatch = ''
                sed -i 's?PathBuf::from("marionette.legato")?PathBuf::from("${./marionette.legato}")?' src/main.rs
              '';

              postInstall = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
                # PipeWire's libjack first, so the JACK host runs on PipeWire without `pw-jack`.
                wrapProgram $out/bin/legato-hand-tracking \
                  --prefix PATH : "${pkgs.lib.makeBinPath [ pkgs.ffmpeg ]}" \
                  --prefix LD_LIBRARY_PATH : "${pkgs.pipewire.jack}/lib:${pkgs.lib.makeLibraryPath wheelLibs}:/run/opengl-driver/lib"
              '';
          });
        in
      {
        inherit legato-hand-tracking legato-hand-tracking-sampler;
      };

      devShells = forEachSystem (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          python = pkgs.python3;

          wheelLibs = wheelLibsPkgs pkgs;
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
