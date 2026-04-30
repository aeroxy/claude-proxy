# Claude Proxy

A local HTTPS MITM proxy specifically designed to optimize the `claude` CLI tool's behavior.

## Features
- Caches Google OAuth tokens locally to speed up execution
- Blocks unnecessary Vertex AI heat-up calls natively
- Transparently routes other traffic via existing Proxies (like Proxyman)

## How to use it

1. Build the proxy:
   ```bash
   cargo build --release
   ```

2. Trust the local CA for Node.js:
   ```bash
   export NODE_EXTRA_CA_CERTS=~/Library/Application\ Support/claude-proxy/ca.crt
   export HTTPS_PROXY=http://127.0.0.1:6666
   ```

3. Run the CLI:
   ```bash
   claude
   ```

## Running as a daemon

```bash
claude-proxy start                    # daemonize on 6666 (or next free port up to 6675)
claude-proxy --port 7000 start        # pick a starting port
claude-proxy stop                     # SIGTERM all running daemons
claude-proxy --port 7000 stop         # stop a specific instance
```

Logs are written to `~/.config/claude-proxy/log/{epoch}.log`, one file per `start`. PID files live at `~/.config/claude-proxy/pids/{port}.pid`.

## Configuration

Config lookup order (first match wins):

1. `HTTPS_PROXY` env var (sets `upstream_proxy` directly)
2. `--config <path>` if provided
3. `./config.toml` in the current working directory
4. `~/.config/claude-proxy/config.toml`
