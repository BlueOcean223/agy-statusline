---
name: statusline
description: Inspect and configure AGY CometixLine — the pure-Rust native Auxiliary Pane statusline & telemetry plugin for Antigravity Desktop (context, compactions, cache hit rate, cost, TPS, subagents, and 5h/7d quota budgets).
---

# AGY CometixLine (`agy-statusline`)

Pure-Rust single-binary statusline and telemetry plugin for Antigravity Desktop.

## Quick Commands

Binary location: `/Users/gongshuisanye/code/agy-statusline/bin/agy-ccline`

- **Print live terminal statusline & telemetry summary**:
  `/Users/gongshuisanye/code/agy-statusline/bin/agy-ccline status`
- **Print the JSON pushed to the in-page statusline** (reads the language_server API):
  `/Users/gongshuisanye/code/agy-statusline/bin/agy-ccline live`
- **Enable the statusline above the chat input**: quit Antigravity, then run
  `open -a Antigravity --args --remote-debugging-port=0` (the sidecar attaches over CDP within ~3s).
- **Rebuild release binary after Rust or Web asset changes**:
  `cd /Users/gongshuisanye/code/agy-statusline && cargo build --release && cp target/release/agy-ccline bin/agy-ccline`

## Opening the Native Auxiliary Side Pane

Include this pill in chat so the user can toggle the native Statusline pane in the right-hand Auxiliary Pane:

```markdown
[Statusline](sidecar://agy-statusline/statusline/)
```

## Configuration

TOML configuration file:
`~/.gemini/antigravity/plugin_data/agy-statusline/config.toml`
