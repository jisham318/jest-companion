# jest-companion

Run [jest-lua](https://github.com/jsdotlua/jest-lua) tests from the command line. My successor to [testez-companion-cli](https://github.com/jackTabsCode/testez-companion-cli)!

## Installation

There are several ways you can install jest-companion and its plugin, but here's how I'd do it with Mise:

Run:

```bash
mise use github:jacktabscode/jest-companion
mise use github:jacktabscode/drillbit # This installs the plugin for you
```

In `drillbit.toml`:

```toml
[plugins.jest-companion]
# This won't auto-update
github = "https://github.com/jackTabsCode/jest-companion/releases/download/v0.2.1/plugin.rbxm"
```

Then, you can run `drillbit` to install the plugin, and `jest-companion` to run your tests in Studio.

## Usage

This tool spins up a server that tells the Studio plugin to run tests, and sends back the results.

Run `jest-companion --help` to see the available options. Several of [jest-lua's runCLI options](https://jsdotlua.github.io/jest-lua/cli) can be set through the CLI, like `--testNamePattern` (which is why I made this tool!)

## MCP server

jest-companion can also run as an [MCP](https://modelcontextprotocol.io) server, so agents like Claude can run your tests through a tool call instead of shelling out to the CLI:

```bash
jest-companion --mcp [path]
```

This keeps the server alive and exposes two tools over stdio:

- `run_tests` — runs the tests in the connected Studio instance and returns the test output. Accepts the same runCLI options as the CLI (`testNamePattern`, `verbose`, `updateSnapshot`, …) plus an optional `projects` array to run a subset of configured projects. Any option you pass overrides the corresponding CLI default.
- `list_projects` — lists the project names configured in `jest-companion.toml`.

Roblox Studio still has to be open with the plugin installed; the MCP server just relays runs to it. A `run_tests` call waits up to `--server-timeout` seconds (default 30) for Studio to pick the run up, then up to `--run-timeout` seconds (default 300) for it to finish.

### Multiple agents at once

Several agents can each run their own `jest-companion --mcp` server simultaneously — each one binds the first free port in a small range (`28861`–`28868`), and the Studio plugin polls them all. Because there's a single Studio instance, runs execute one at a time: if a second run is requested while another is still going, it queues and runs as soon as Studio is free. A wedged or leftover MCP server only ties up its own port, so it never blocks the others.

Register it with Claude Code like so:

```bash
claude mcp add jest-companion -- jest-companion --mcp /path/to/your/project
```

Or, for any MCP client, add an entry like:

```json
{
  "mcpServers": {
    "jest-companion": {
      "command": "jest-companion",
      "args": ["--mcp", "/path/to/your/project"]
    }
  }
}
```

## Notes

- The plugin does not forward logs to the CLI. See the Studio output for these.
- The CLI and the plugin negotiate a protocol version, so keep them on matching versions. (This release uses protocol version 3.)
