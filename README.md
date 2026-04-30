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
   export NODE_EXTRA_CA_CERTS=~/.config/claude-proxy/ca.crt
   export HTTPS_PROXY=http://127.0.0.1:6666
   ```

3. Run the CLI:
   ```bash
   claude
   ```
