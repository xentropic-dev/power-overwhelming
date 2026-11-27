# Dashboard

The dashboard is a Rust WebAssembly component that ships with Power. It registers the `dashboard.open` command, bound to Ctrl+B then `o`, and draws a panel from the daemon's overview: totals with CPU and RAM, one line per active or stalled DAG run, model usage limits, and one line per box. Its [source](src/guest.rs) owns the layout. The [panel WIT contract](../wit/panel.wit) defines the boundary, and Power draws the returned lines, text and bars with native controls.

## Build the component

Run these commands from the repository root with the Rust toolchain and `wasm-tools` installed.

```sh
rustup target add wasm32-unknown-unknown
cargo build --locked -p dashboard --release --target wasm32-unknown-unknown
wasm-tools component new target/wasm32-unknown-unknown/release/dashboard.wasm \
  -o plugins/dashboard/dashboard.wasm
wasm-tools validate plugins/dashboard/dashboard.wasm
```

The desktop embeds the built component. Regenerate it when changing the guest source, its defaults or the WIT. `cargo test -p dashboard` runs the text helpers natively.

## Settings

Set `dashboard` in `${XDG_CONFIG_HOME:-~/.config}/power-overwhelming/config.lua` and press Ctrl+B then `r` to reload. [Bundled defaults](defaults.json)

```lua
dashboard.refresh_ms = 5000   -- how often an open dashboard refreshes, default 2000, at least 250
keys.box["dashboard.open"] = "g"
```

## Implement another panel plugin

Use `wit_bindgen::generate!` with both WIT files and the `power:panel/panel-plugin` world, implement its `Guest` trait, then export your implementation with `export!`.

- `settings` can supply a Lua section name and its default JSON object. `configure` receives the resolved section as JSON and can return a refresh interval in milliseconds for the plugin's open panels.
- `commands` lists commands with namespaced ids such as `myplugin.open`, a scope (`box` or `tab`) and default keys. They appear in the command palette and the shortcut help, and users rebind them with `keys.box["myplugin.open"]`.
- `invoke` runs a command. Return `open-panel(id)` to open a panel.
- `render-panel` draws an open panel from the overview, fitted to a width in terminal columns. A line has left and right items: text segments, bars with a 0–1 ratio and a width in columns, and spacers. Colors use the same names and hex values as Power-bar segments. `palette.get-palette` returns the active scheme during this call.

Put the component in the `plugins` folder next to `config.lua` and press Ctrl+B then `r`.
