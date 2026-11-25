# woomer

This is a fork of [coffeeispower/woomer](https://github.com/coffeeispower/woomer), which is itself inspired by tsoding's [boomer](https://github.com/tsoding/boomer). The main difference is that it runs as a `wlr-layer-shell` **overlay** instead of a normal window, this makes it work well in niri (at least for me)


## Options

```
woomer [OPTIONS]

--monitor <name>     Monitor the overlay is shown on. Defaults to the first one
--output <name>      Monitor to capture. Captures all monitors if not specified
--radius <number>    Flashlight radius. Bigger number means bigger radius. Defaults to 1
-S, --show-cursor    Include the cursor in the capture. Off by default
```

## Installing

### Nix (flakes)

```nix
# flake.nix
{
  inputs = {
    woomer.url = "github:The-Beaconguy/my-woomer";
    # .....
  }
}
```

Then install it 

```nix
modules = [
    ({pkgs, ...}: {
      environment.systemPackages = [
        inputs.woomer.packages.${system}.default
      ];
    })
```

Can you use Home-manger as well

```nix 
{inputs, system, ...}:
{
  home.packages = [
    inputs.woomer.packages.${system}.default
  ];
}
```

### From source

Build dependencies (development files, found via pkg-config and the linker), plus a C toolchain:
wayland, xkbcommon, EGL and gbm.

```
cargo build --release
./target/release/woomer
```

At runtime you need a working OpenGL 3.3 driver and a compositor that supports `wlr-layer-shell` and screen capture. And as of writing this, GNOME's compositor does not support layer-shell, so woomer won't run there.

## Binding it to a key

```
# niri (config.kdl)
binds {
    Mod+Z { spawn "woomer"; }
}
```

The overlay's layer-shell namespace is `woomer`, in case you want to target it with a layer rule.

## Changes from upstream

- Replaced raylib/GLFW with a native layer-shell overlay (smithay-client-toolkit) rendered with OpenGL 3.3 through EGL
- Switched `libwayshot` from a git branch that no longer exists to the released crate
- Fractional scaling through `wp_fractional_scale` and `wp_viewporter`
- Multiplicative zoom, limited to 0.01x - 200x
- Mirror (`M`) keeps what is on screen in place when toggled
- vertical flip (`V`) 
  So now `V` + `M`  gives 180° rotation
- Flashlight: `--radius` applies from startup, the shrinking pulse plays when it is enabled with `F`, and its growth with zoom is soft-capped (`SPOTLIGHT_ZOOM_CAP`)


The shaders are embedded into the binary at build time. With `cargo build --features dev`, `spotlight.fs` is read from disk instead, so you can edit it without rebuilding.

## License

MIT, as upstream.
