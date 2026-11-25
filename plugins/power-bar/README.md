# Power-bar

Power-bar is a Rust WebAssembly component that supplies the desktop's `terminal.bottom` row. Its [source](src/lib.rs) owns the templates, segment order, defaults, clock request, and clipboard menu. Power supplies a view snapshot and draws the result with native controls. The [WIT contract](../wit/terminal-ui.wit) defines the boundary.

## Build the component

Run these commands from the repository root with the Rust toolchain and `wasm-tools` installed.

```sh
rustup target add wasm32-unknown-unknown
cargo build --locked -p power-bar --release --target wasm32-unknown-unknown
wasm-tools component new target/wasm32-unknown-unknown/release/power_bar.wasm \
  -o plugins/power-bar/power-bar.wasm
wasm-tools validate plugins/power-bar/power-bar.wasm
```

The desktop embeds the built component. Regenerate it when changing the guest source or its defaults.

## Implement another bottom-row plugin

The [minimal plugin](examples/minimal.rs) provides one live segment. Use `wit_bindgen::generate!` for the `terminal-bottom` world, implement its `Guest` trait, then export your implementation with `export!`. The three callbacks cover the lifecycle.

- `settings` can supply a Lua section name and its default JSON object. Return `None` for a plugin without settings.
- `configure` receives the resolved settings as JSON. Return a strftime format if the row uses a local clock.
- `render` receives the active view, optional contributed fields, and the requested clock value. Return a row with left and right segments and an optional action menu. The host handles fitting the row and can copy a declared value when the user chooses it. Return `None` to hide the row.

Use `{theme.name}` in a Power-bar template to show the active scheme. Plugins that need its exact RGB values can target `terminal-bottom-with-palette` and call `palette.get-palette()` during `render`. It reads the same palette the desktop uses. Power calls `render` after a scheme change, and plugins can keep returning color names for the host to resolve.

A segment can request `path` overflow to preserve the tail, `text` to trim the end, or `keep` for higher priority. Foreground and background accept terminal color names, `foreground`, `background`, `accent`, `muted`, `surface`, `raised`, `contrast`, and hex RGB.

Send `` for Powerline chevrons or `` for rounded joins. Power mirrors the join on the right and fills it with the adjacent segment colors. It draws these joins directly, so they also work with fonts that lack Powerline glyphs. Other separator strings render as Unicode text.

To try the example, build it with `cargo build --locked -p power-bar --example minimal --release --target wasm32-unknown-unknown` and use `wasm-tools component new` on `target/wasm32-unknown-unknown/release/examples/minimal.wasm`.

Place your component at `${XDG_CONFIG_HOME:-~/.config}/power-overwhelming/terminal-bottom.wasm`. Ctrl+B then `r` reloads it. Ctrl+S then `y` opens its action menu when it supplies one.
