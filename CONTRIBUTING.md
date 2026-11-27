# Contributing to Power Overwhelming

Contributions are welcome. Bug reports, documentation, performance fixes, and improvements to everyday terminal workflows all help.

## Design principles

### Start with the terminal

Give the focused terminal the space it needs. Add tools that make terminal work more effective. The WebKit preview pane is one example. An engineer can open an agent's local web page beside the terminal, inspect it, and return to the conversation with the keyboard.

### Keep engineers in charge

Power helps engineers direct agent work and inspect the results. Make it easy to see what an agent is doing, open its terminal and worktree, review what it published, and answer when it needs a decision. Design those paths around the person doing the work.

### Design for the keyboard

Every desktop action should be reachable from the keyboard. Keep ordinary keystrokes available to the terminal. Focus a picker's search field when it opens, and give focus back to the invoking view on Escape. Make commands discoverable from the shortcut list.

### Keep it native and fast

Keep the desktop shell, terminal, and navigation in native GPUI on Wayland. Use WebKit for web previews. Typing, scrolling, resizing, and switching sessions should feel immediate, including when workers and plugins are busy. Keep blocking work off the UI thread and switch views without transition animations.

### Give useful tools a beautiful form

Keep everyday views sparse and put details behind a picker or panel the user can open on demand. Use spacing, alignment, square corners, and contrast to make the current selection, keyboard focus, and requests for attention easy to see. Take the same care with typography and color in the terminal, tabs, and previews.

## Sending a change

- Describe the workflow you want to improve or give steps to reproduce the bug.
- Follow the [build instructions](README.md#build-and-run). Check the [architecture notes](README.md#architecture) for the workspace layout and test commands.
- In your pull request, say what changed and how you checked it. For a desktop change, include the keyboard path and a screenshot or short recording if it helps explain the result.
