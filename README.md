# Power Overwhelming

A native Wayland desktop multiplexer for coding agents such as Claude Code, Codex, and Grok. The local daemon keeps terminals and agents running when the GPUI window closes. Artifact links open in a closable pane beside the terminal.

## Build and run

Use Rust 1.97 or later and Zig 0.17.x. The Linux build uses a C compiler, pkg-config, WebKitGTK 4.1, JSON-GLib, and GStreamer application and video libraries. Playback also needs GStreamer codecs for MP4 and WebM. GPUI requires a working Vulkan driver.

```sh
mise run release
./target/release/power gui
```

Run `power` or `power gui` from any directory. Enter a Git checkout path in the picker to create a box with a shell tab. As you type, the picker searches every directory up to six levels below the path before the last separator with fzf’s algorithm and query syntax: space-separated terms must all match, and `'exact`, `^prefix`, `suffix$`, and `!exclude` work as in fzf. Lowercase queries ignore case. Matches in the final folder name rank first. The search skips hidden directories and anything `.gitignore` excludes, and does not enter `node_modules` or `target`. Tab completes the selected result, and Enter opens it. Type `codex`, `claude`, or `grok` in the shell to start an installed agent. Tabs show their number and what is running, such as `1 shell` or `2 claude`, and update when an agent starts or exits.

## Shell integration

On first launch, Power offers to add one line to your shell's rc file (`~/.bashrc`, `~/.zshrc`, or fish's `config.fish`). Inside Power terminals, that line defines a shell function for each agent profile. Profiles with launch instructions tell agents how to load bundled Power skills (see [Skills](#skills)). Outside Power terminals, the line does nothing. To install it later, press `ctrl-b i` in the app or run `power setup` (`power setup --dry-run` shows the change first). To remove it, delete the line and the comment above it. Power restores the last open box and its active tab when you relaunch with the daemon still running.

Press and release Ctrl+B for box commands or Ctrl+S for tab commands, then press the command key. Ctrl+B then `?` lists the shortcuts. Ctrl+B then `x` detaches a box and Ctrl+B then `s` reopens it. Ctrl+S then `c` opens a shell tab, `t` picks an existing worktree, `r` recovers an unassigned session in the repository, and `x` opens the end-session confirmation. Use `power session list` and `power session end <id>` to manage sessions from the CLI.

## Skills

Power bundles skills that teach agents its workflows: `power-dag` for DAG orchestration and `power-artifacts` for publishing previews. They ship inside the `power` binary, so the guide always matches the release, and reading them needs no daemon.

```sh
power skills list                           # name and description of each skill
power skills get power-dag                  # the guide
power skills get power-dag --references     # list reference names
power skills get power-dag --reference worker-contract
power skills get power-dag --full           # guide plus every reference
```

Add `--json` to any of these for machine-readable output.

### Discovery

Agents find skills in two ways. `power --help`, `power dag --help`, and `power skills --help` end with a short footer that points agents at `power skills list`. Agents that Power starts with a profile that has launch instructions, from a shell wrapper, `power session start`, or `power dag dispatch`, also get a short block naming each bundled skill and how to load it, plus a request to label the session with `power session task`. Skills already installed in the agent's native skill folder are left out of that block, and when every skill is installed, the block keeps only the `power session task` line.

### Native installs

`power skills install` writes a small stub `SKILL.md` for each bundled skill so agents can find it without Power's launch instructions. The stub tells the agent to load the full guide with `power skills get <name>`, so installs stay current when Power updates.

```sh
power skills install --dry-run   # show what would change
power skills install             # user scope: ~/.agents/skills
power skills install --local     # project scope: .agents/skills in the current Git worktree
power skills update              # refresh existing installs; never adds new locations
```

Stubs go in the shared `.agents/skills` folder, which Codex and Grok read natively. For every other detected agent profile with a `skills_dir`, Power links `<skills_dir>/<name>` to the shared stub. In user scope an agent counts as detected when the parent of its `skills_dir` exists (for Claude Code, `~/.claude`). In project scope the worktree must already contain the profile's skill folder, such as `.claude/skills`. Use `--skill <name>` to select skills and `--agent <id>` to select profiles by ID, or `--agent universal` for the shared folder alone. `--json` prints each location's outcome.

Power only replaces what it wrote. It rewrites a stub when the file matches a stub from some Power release, and replaces a link only when it points at the shared copy. A stub you edited, a folder with other files, or a skill another package installed under the same name is left alone with a warning. Pass `--force` to replace those. Power moves the old copy to `~/.agents/.power-skill-backups` first.

## Agent profiles

Power provides `claude`, `codex`, and `grok` profiles. Pass a profile ID to `power session start`, `power agent start --provider`, or `power dag dispatch --provider`. With shell integration, use the ID as a command inside a Power terminal.

Add profiles or override built-in fields in `${XDG_CONFIG_HOME:-~/.config}/power-overwhelming/config.lua`. For example, this sets every field of the Grok profile. The built-in profile skips more subcommands.

```lua
agents.grok = {
  command = "grok",
  injection = { kind = "flag", flag = "--rules" },
  skip_subcommands = { "login", "logout", "mcp", "update", "version" },
  skip_if_args_contain = { "--rules" },
  skills_dir = "~/.agents/skills",
}
```

A new profile for another agent, and an override of one built-in field:

```lua
agents["my-agent"] = {
  command = "/opt/bin/my-agent",
  injection = { kind = "flag", flag = "--append-system-prompt" },
  skip_subcommands = { "login", "version" },
  skip_if_args_contain = { "--append-system-prompt" },
  skills_dir = "~/.my-agent/skills",
}

agents.codex = { command = "/opt/bin/codex" }
```

For a new profile, `command` defaults to the ID and `injection` defaults to `{ kind = "none" }`. Use the executable's name on PATH or an absolute path. Profile IDs use lowercase letters, digits, hyphens and underscores and start with a letter or digit. Power reserves `shell`, `command`, and `power`.

Use `{ kind = "config_value", flag = "-c", key = "developer_instructions" }` to pass launch instructions as a JSON-encoded string in a `key=value` argument. The skip lists match the first argument exactly for subcommands and look for substrings across all arguments for existing flags or settings.

`skills_dir` is the folder where the agent reads user skills natively; a leading `~` means your home directory. The same path relative to home is the project skill folder, so `~/.claude/skills` also means `.claude/skills` in the worktree and its parents. `power skills install` links skills into it, and launch instructions leave out skills already there. Set it to `~/.agents/skills` for an agent that reads the shared folder, and leave it out when the agent has no skill folder.

Open a new shell to pick up new wrapper names. Restart the daemon when you want profile changes to apply to daemon-launched agents.

## Color schemes

Choose a scheme on first launch, or press Ctrl+B then `t` to change it later. Type in the picker to search scheme names and aliases. The default is VS Code Dark or VS Code Light, based on the desktop appearance. Power bundles the full [WezTerm catalog](https://wezterm.org/colorschemes/index.html) and applies the selected palette to its native controls, running terminal sessions, and Power-bar.

The picker remembers your choice. You can also set the scheme in `config.lua`.

```lua
appearance.scheme = "Catppuccin Latte"
```

The Lua setting takes precedence at startup and when you reload with Ctrl+B then `r`. A picker selection applies immediately. [Palette sources](infrastructure/themes/README.md)

## DAG orchestration

Run `power skills get power-dag` to load the bundled coordinator skill, or `power dag guide` (an alias for `power skills get power-dag --full`) to print it with all references, including a three-task example. From a Power terminal, `power dag list` finds runs in the current box. From an external terminal, use `power box list` and pass `--box` when listing or creating a run, plus `--coordinator` on creation. Create a run with `power dag create`, add tasks and dependencies with `power dag apply`, then use `power dag ready` and `power dag dispatch --provider <profile id>` to start background workers in existing worktrees. Follow results with `power dag wait --since` and explicitly retry failed or interrupted work with `power dag retry`.

`power dag create` and `power dag show` return the run and a `pwr://dag/<run-id>` link. Coordinators share that link verbatim on its own line when they create or reopen a run. Click the link or open it with Ctrl+S then `o` to see the live graph.

Press Ctrl+B then `d` to browse the active box's task graphs or Ctrl+B then `a` to open its agent tree. Attach to a worker from either view without interrupting its work. An input request highlights the box and the worker's tab when attached. Answer in that worker's terminal. Its highlight clears when it reports that the request is resolved, and the box highlight clears when its last request is resolved.

## Power-bar

The status bar shows the main checkout and active worktree folder names with distinct icons, followed by the foreground agent and its task when reported. Press Ctrl+S then `y` to copy either full path. Power’s launch instructions ask agents to set `{agent.task}` with `power session task "Fix WebM playback"` and update it as their work changes. An integration running under the agent can report a model with `power session model "Model name"`.

Set `power_bar` in `${XDG_CONFIG_HOME:-~/.config}/power-overwhelming/config.lua` and press Ctrl+B then `r` to reload. The default uses colored Powerline joins and changes the rightmost accent with the active agent. Set `separator` to `""` for rounded joins or to Unicode text such as `" │ "` for a plain separator. Each segment can set an `icon`, foreground, and background. [Bundled defaults](plugins/power-bar/defaults.json)

```lua
power_bar.separator = ""
power_bar.left = {
  { icon = "▣", text = "{repo.path}", fg = "contrast", bg = "accent", bold = true },
  { icon = "", text = "{worktree.path}", bg = "raised" },
}
power_bar.agent_colors.Codex = "#62a0ea"
power_bar.right = {
  { text = "{agent.task}", bg = "surface" },
  { text = "{agent.model}", fg = "muted", bg = "surface" },
  { icon = "◷", text = "{time}", fg = "muted", bg = "raised" },
  { text = "{agent.type}", fg = "contrast", bg = "agent", bold = true },
}
```

The bar omits segments with unavailable fields. Set `power_bar.enabled = false` to hide it. Power-bar is a [bundled Wasm plugin](plugins/power-bar/README.md). To replace it, put a component implementing the [terminal UI interface](plugins/wit/terminal-ui.wit) next to `config.lua` as `terminal-bottom.wasm` and reload.

To add template fields, put components implementing the [status fields interface](infrastructure/wit/status-bar.wit) in the `status-plugins` folder next to `config.lua`, or set `POWER_STATUS_PLUGIN_DIR` before starting the desktop. With the optional plugin host built, Power-bar reads their values as `{custom.name}`. Values defined in `power_bar.params` take precedence.

## Key bindings

Every action has a keyboard binding. Ctrl+B or Ctrl+S then `?` lists the current bindings. Override any of them in `~/.config/power-overwhelming/config.lua`. Entries you leave out keep their defaults:

```lua
keys.tab.next = { "n", "j" }   -- one key or a list
keys.prefix.tab = "ctrl-a"
keys.preview.close = "q"

previews.auto_open = "always"  -- "when_pane_open" (default) or "never"
previews.history = 50          -- artifacts the link search lists, default 20
```

With `auto_open = "when_pane_open"`, an artifact an agent publishes replaces or joins the previews only when that box already shows its preview pane. With `"always"`, a publish also opens the pane. Auto-open never moves keyboard focus away from the terminal.

The key sections are `prefix`, `box`, `tab`, `picker`, `dialog` (pickers without a search field), `preview` and `hints`. Ctrl+B then `r` reloads the file. If the file has a typo, an unknown command, a clash between two commands, or a value of the wrong type, Power names the entry and keeps the bindings it was already using.

## Preview an artifact

Click a local PNG, JPEG, WebP, MP4, WebM, or HTML link in the terminal, or open it from the keyboard. Ctrl+S then `o` numbers up to nine links on screen, newest output first. Press `1`–`9` to open and focus a preview, or Esc to back out of the numbered links. Ctrl+S then `o` then `s` searches this box's published artifacts, newest first, along with any other links on screen. Selecting a preview from the search also focuses it. Ctrl+S then `v` focuses the preview pane, including after typing into a web page. In the pane, `h`/`l` switch previews, Esc or `x` closes the active preview, and Tab returns to the terminal with the preview open. Space plays or pauses video, `,`/`.` seek, `j`/`k` scroll a page, `r` reloads it, and `i` types into it. Loopback URLs such as `http://localhost:3000/` open in the same pane. Shift-drag selects text when the terminal app is handling mouse events. Ctrl-Shift-C and Ctrl-Shift-V copy and paste.

Run `power skills get power-artifacts` for the full guide to publishing artifacts and sharing their links. From any Power terminal, you can publish a file or a running server.

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
POWER_PLUGIN_DIR=/absolute/path/to/plugins ./target/release/power gui
```

Set `POWER_PLUGIN_DIR` in the daemon's environment. A GUI that attaches to an existing daemon uses that daemon's configuration. The directory opts its regular `.wasm` files into the [versioned artifact WIT interface](infrastructure/wit/artifact-plugin.wit). The host loads these components on the first publish, delivers best-effort notifications after the database write, and allows each callback to resolve the current artifact. Restart the daemon to reload the directory. The notification path uses a bounded background queue and the separate host applies execution, memory, and query limits.

To compare daemon startup, reattachment, and PTY round-trip latency on the same machine, run the benchmark against both builds. Pass `--plugin-dir` to exercise artifact delivery during input.

```sh
python3 scripts/bench-daemon.py ./target/release/power --reference /path/to/previous/power
```
