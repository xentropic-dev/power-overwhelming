# Power Overwhelming

A native Wayland desktop multiplexer for Codex and Claude Code. The local daemon keeps terminals and agents running when the GPUI window closes. Artifact links open in a closable pane beside the terminal.

## Build and run

Use Rust 1.96 or later and Zig 0.17.x. The Linux build uses a C compiler, pkg-config, WebKitGTK 4.1, JSON-GLib, and GStreamer application and video libraries. Playback also needs GStreamer codecs for MP4 and WebM. GPUI requires a working Vulkan driver.

```sh
mise exec zig@0.17.1 -- cargo build --release --locked
./target/release/power gui /path/to/repository
```

Choose a Git worktree and start a shell, Codex, or Claude from the toolbar. The agent buttons use the installed CLIs and their existing sign-ins. Click **×** on a session tab to end that session. Closing the window detaches from the sessions. Reopen it with the same command to reattach. Use `power session list` and `power session end <id>` to manage sessions from the CLI.

## Preview an artifact

Click a local PNG, JPEG, WebP, MP4, WebM, or HTML link in the terminal. Loopback URLs such as `http://localhost:3000/` open in the same pane. Shift-drag selects text when the terminal app is handling mouse events. Ctrl-Shift-C and Ctrl-Shift-V copy and paste.

Agents launched from the app receive instructions to publish an artifact and include the returned link in their reply. From any Power terminal, you can do the same.

```sh
power artifact publish './out/my video.mp4' --title 'Demo'
power artifact publish http://localhost:3000/
```

Publishing returns a `pwr://artifact/…` link backed by a reference to the file or server. Keep the file in place and the preview server running while you want the link to work. HTML file previews serve sibling assets from the file’s directory. The browser helper runs offscreen on Wayland and uses an ephemeral browsing context.

The vendored WebKitGTK helper comes from Zeron. Its license is in [vendor/webkit/LICENSE](vendor/webkit/LICENSE).

## Architecture

The Cargo workspace has four layers. `domain` owns session and artifact rules. `application` owns commands, queries, and the interfaces they use. `infrastructure` implements those interfaces and owns the daemon, its Unix socket, SQLite, and the PTY runtime. `presentation` contains the `power` binary, CLI, and GPUI desktop. Terminal input and subscription streaming use the runtime directly.

```sh
cargo test --locked -p domain -p application
mise exec zig@0.17.1 -- cargo test --workspace --locked
```

## Artifact plugins

Build the optional host and put it next to the `power` binary. The plugin host build needs Rust 1.97 or later.

```sh
mise exec zig@0.17.1 -- cargo build --release --locked -p infrastructure --features wasm-plugins --bin power-plugin-host
POWER_PLUGIN_DIR=/absolute/path/to/plugins ./target/release/power gui /path/to/repository
```

Set `POWER_PLUGIN_DIR` in the daemon's environment. A GUI that attaches to an existing daemon uses that daemon's configuration. The directory opts its regular `.wasm` files into the [versioned artifact WIT interface](infrastructure/wit/artifact-plugin.wit). The host loads these components on the first publish, delivers best-effort notifications after the database write, and allows each callback to resolve the current artifact. Restart the daemon to reload the directory. The notification path uses a bounded background queue and the separate host applies execution, memory, and query limits.

To compare daemon startup, reattachment, and PTY round-trip latency on the same machine, run the benchmark against both builds. Pass `--plugin-dir` to exercise artifact delivery during input.

```sh
python3 scripts/bench-daemon.py ./target/release/power --reference /path/to/previous/power
```
