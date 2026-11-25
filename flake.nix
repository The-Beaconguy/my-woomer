{
  description = "Zoomer application for Wayland inspired by tsoding's boomer";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    systems.url = "github:nix-systems/default";

    crane.url = "github:ipetkov/crane";
  };

  outputs = {
    self,
    nixpkgs,
    systems,
    crane,
    ...
  }: let
    forEachSystem = nixpkgs.lib.genAttrs (import systems);
  in {
    checks = forEachSystem (system: {
      inherit (self.packages.${system}) woomer;
    });

    packages = forEachSystem (system: let
      pkgs = nixpkgs.legacyPackages.${system};
      craneLib = crane.mkLib pkgs;

      # Libraries woomer links against (wayland-client/egl, libxkbcommon, libEGL, libgbm).
      # pkg-config and the linker find them through these.
      buildInputs = with pkgs; [
        wayland
        libxkbcommon
        libglvnd
        libgbm
      ];

      # Common arguments can be set here to avoid repeating them later
      # Note: changes here will rebuild all dependency crates
      commonArgs = {
        src = let
          # The GLSL shaders (*.fs and *.vs) are include_str!'d by the Rust code,
          # so they must be part of the source that crane builds.
          shaderFilter = path: _type: builtins.match ".*\\.(fs|vs)$" path != null;
          shaderOrCargo = path: type:
            (shaderFilter path type) || (craneLib.filterCargoSources path type);
        in
          pkgs.lib.cleanSourceWith {
            src = craneLib.path ./.;
            filter = shaderOrCargo;
          };
        strictDeps = true;
        nativeBuildInputs = [pkgs.pkg-config];
        inherit buildInputs;
      };
    in {
      woomer = self.packages.${system}.default;
      default = craneLib.buildPackage (commonArgs
        // {
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          postFixup = ''
            patchelf $out/bin/woomer \
              --add-rpath ${pkgs.lib.makeLibraryPath buildInputs}
          '';

          meta = {
            description = "Zoomer application for Wayland inspired by tsoding's boomer";
            license = pkgs.lib.licenses.mit;
            mainProgram = "woomer";
            platforms = pkgs.lib.platforms.linux;
          };
        });
    });

    apps = forEachSystem (system: let
      pkgs = nixpkgs.legacyPackages.${system};
    in {
      default = {
        type = "app";
        program = pkgs.lib.getExe self.packages.${system}.default;
      };
    });

    devShells = forEachSystem (system: let
      pkgs = nixpkgs.legacyPackages.${system};
      craneLib = crane.mkLib pkgs;
    in {
      default = craneLib.devShell {
        checks = self.checks.${system};
        packages = with pkgs; [
          rust-analyzer
        ];
        # `cargo run` inside the shell isn't patched like the packaged binary,
        # so let it find libEGL and friends at runtime.
        env = {
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (with pkgs; [
            wayland
            libxkbcommon
            libglvnd
            libgbm
          ]);
        };
        inputsFrom = [
          self.packages.${system}.default
        ];
      };
    });
  };
}
