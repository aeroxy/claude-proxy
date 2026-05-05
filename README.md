# Claude Proxy

A local HTTPS MITM proxy specifically designed to optimize the `claude` CLI tool's behavior.

## Features
- Caches Google OAuth tokens locally to speed up execution
- Blocks unnecessary Vertex AI heat-up calls natively
- Deduplicates byte-identical concurrent requests so duplicates don't burn upstream tokens
- Auto-recovers from expired credentials: when Google returns `invalid_grant`, opens a browser, runs the consent flow, writes a fresh ADC, and resumes the in-flight request transparently (see [wiki/auto-reauth.md](https://github.com/aero/claude-proxy/blob/master/wiki/auto-reauth.md))
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
claude-proxy restart                  # stop + start (no-op stop if nothing running)
claude-proxy --port 7000 restart      # restart a specific instance
```

Logs are written to `~/.config/claude-proxy/log/{epoch}.log`, one file per `start`. PID files live at `~/.config/claude-proxy/pids/{port}.pid`.

## Configuration

Config lookup order (first match wins):

1. `--config <path>` if provided
2. `./config.toml` in the current working directory
3. `~/.config/claude-proxy/config.toml`

`HTTPS_PROXY` is **not** read for `upstream_proxy` — it's a client-side var meant to point clients at this proxy, and reading it here would make the proxy chain through itself when `HTTPS_PROXY=http://127.0.0.1:6666` is set in the same shell. Configure chained proxies (Proxyman, mitmproxy) explicitly via `upstream_proxy = "..."` in `config.toml`.
