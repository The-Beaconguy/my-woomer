{
  pkgs,
  lib,
  ...
}: {
  languages.rust.enable = true;

  packages = with pkgs; [
    jetbrains.rust-rover
    wayland
    libxkbcommon
    libglvnd
    libgbm
    libdrm
    mesa
    pkg-config
  ];
  env.LD_LIBRARY_PATH =
    lib.makeLibraryPath [
      pkgs.libglvnd
      pkgs.wayland
      pkgs.libxkbcommon
    ]
    + ":/run/opengl-driver/lib";
}
