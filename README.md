# Project Stratagem

Project Stratagem is a native Windows desktop control panel for the local Ollama gateway stack. The interface and process manager are written in Rust and rendered with eframe/egui. It is a real desktop executable, not a browser-based dashboard or a Python wrapper.

## What it manages

- Ollama: detect port 11434, launch `ollama serve`, and test `/api/tags` to list installed models
- Node bridge: launch the existing `bridge.mjs` with the configured token environment variable
- Cloudflare Tunnel: launch `cloudflared tunnel --url <target>` and write process output to its own log
- Process control: stop only processes started by Stratagem; services already running before launch are left alone
- Configuration: save executable paths and local URLs under `%LOCALAPPDATA%\ProjectStratagem`
- Token protection: encrypt the bridge token with Windows DPAPI for the current Windows account
- Activity: view application events and open the individual service logs

The app includes a redesigned command center, sidebar navigation, service-health indicators, quick actions, a Cloudflare Worker connection panel, Settings, and Activity. The Dashboard includes a safe Restart Bridge action that only stops the Node.js process whose command line matches the configured bridge.mjs path, then relaunches it with the saved token. A chess king icon is used for the app window and Windows executable. Native Windows clipboard integration is enabled for Ctrl+C, Ctrl+X, Ctrl+V, and the app's Copy buttons. The Windows build is a standalone `Project-Stratagem.exe`; Ollama, Node.js, cloudflared, and your existing bridge script remain separate prerequisites.

## Requirements

- Windows 10 or Windows 11 (64-bit)
- [Ollama](https://ollama.com/download)
- [Node.js](https://nodejs.org/)
- [Cloudflare Tunnel / cloudflared](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/)
- Your existing `bridge.mjs` gateway script

## Clipboard support

The Windows desktop build enables eframe's OS clipboard integration. Text fields support the normal Windows shortcuts (`Ctrl+C`, `Ctrl+X`, and `Ctrl+V`), and buttons that copy URLs, tokens, or logs write to the Windows clipboard rather than an app-local fallback.

## Connection diagnosis (when Clank World says the key is wrong)

A Clank World request crosses four hops, and each has its own credential:

```
Clank World --(API_KEY)--> Worker --(X-Bridge-Token = BRIDGE_TOKEN)--> Quick Tunnel --> bridge.mjs --> Ollama
```

A `401` in Clank World can come from **two different places**: the Worker rejecting the API key, or the bridge rejecting the Worker's `BRIDGE_TOKEN` (the Worker passes that 401 back). They look the same from Clank World.

In **Settings**, enter the Clank World key and click **Test Worker Key**. Stratagem tests each hop in order and names the first one that fails:

| Result | What it means | Fix |
| --- | --- | --- |
| Hop 1 rejected | The running bridge uses a different token (often an old bridge still running) | **Restart Bridge** |
| Hop 1: Bearer works, X-Bridge-Token doesn't | bridge.mjs checks the wrong header | Make bridge.mjs accept `X-Bridge-Token` |
| Hop 2 fails | Tunnel is down or still starting | Wait or restart the tunnel |
| Hop 3: Worker rejected the key | Key doesn't match the Worker's `API_KEY` | `npx wrangler secret put API_KEY` |
| Hop 3: key passed, bridge rejected | Worker `BRIDGE_TOKEN` doesn't match Stratagem's token | `npx wrangler secret put BRIDGE_TOKEN` |
| Hop 3 unreachable | Worker `BRIDGE_URL` points at an old tunnel | Set `BRIDGE_URL` to the current tunnel URL |

To tell the two 401s apart, it also sends a deliberately wrong key and compares the answers. The key you test with stays in memory only. It is never saved or logged.

Common gotchas:

- **Every Quick Tunnel restart gets a new URL.** Each time the tunnel restarts, update the Worker's `BRIDGE_URL`.
- **Don't pipe secrets with `echo`.** `echo abc | wrangler secret put API_KEY` stores `abc` plus a newline. Type the value at the prompt instead.
- Base URL in Clank World is the Worker URL plus `/v1`. Stratagem now strips extra `/v1` or `/chat/completions` you paste into Settings.

**Restart Bridge (apply saved token)** stops only a `node.exe` listener on port 11435 that runs `bridge.mjs`. The path check ignores slash direction and letter case, and it also catches a bridge started as `node bridge.mjs` from its own folder. Unrelated listeners are left alone and listed in Activity.

## Build from source

Install the Rust stable toolchain, then run from the repository root:

```powershell
cargo build --release
.\target\release\Project-Stratagem.exe
```

The executable is written to `target\release\Project-Stratagem.exe`.

## Download a Windows build

Open the repository's Releases page to download the latest Windows executable or portable ZIP. Tagged versions automatically run formatting, compilation, tests, package the app, and create a prerelease with the Windows assets.

## First launch

1. Open **Settings** and verify the path to your existing `bridge.mjs`.
2. Verify the Node.js and `cloudflared.exe` paths. Stratagem searches the current `PATH` automatically, but you can browse to each executable.
3. Set the token environment-variable name to what your bridge actually reads. It defaults to `BRIDGE_TOKEN`.
4. Set the local bridge token to match the Cloudflare Worker `BRIDGE_TOKEN` secret. The app's Show and Copy controls let you retrieve it.
5. Select **Save encrypted token**, then **Save Settings**.
6. Return to **Dashboard** and select **Start All**.
7. In **Settings**, set the Cloudflare Worker URL (default: `https://ollama-gateway.pizzadoe173.workers.dev`), enter your Clank World key, and click **Test Worker Key**.
8. In Clank World, use the Worker URL with `/v1` appended as the Base URL. The API Key must be the Worker `API_KEY` secret, not the local bridge URL or bridge token. The Worker forwards requests using its own `BRIDGE_TOKEN` secret.

The default Ollama URL is `http://127.0.0.1:11434`; the default bridge/tunnel target is `http://127.0.0.1:11435`.

## Cloudflare note

Stratagem starts a **Quick Tunnel**, which creates a temporary public URL. It records cloudflared output in `%LOCALAPPDATA%\ProjectStratagem\logs\tunnel.log`. It does not edit your Cloudflare Worker URL, Worker secret, or remote gateway configuration. Those settings still need to match the active tunnel and bridge token.

## Data and logs

- Settings: `%LOCALAPPDATA%\ProjectStratagem\settings.json`
- Encrypted token: `%LOCALAPPDATA%\ProjectStratagem\token.dpapi`
- Application log: `%LOCALAPPDATA%\ProjectStratagem\logs\stratagem.log`
- Service logs: `ollama.log`, `bridge.log`, and `tunnel.log` in the same logs directory

The token file is protected with Windows DPAPI and is intended to be decrypted by the same Windows user account on the same machine. Do not expect it to be portable between Windows accounts or PCs.

## Safety behavior

- Stratagem checks whether ports 11434 and 11435 are already responding before starting duplicate services.
- **Stop All** terminates only child processes that Stratagem itself launched.
- Existing Ollama services or bridge processes are not terminated.
- Service stdout and stderr are written to separate log files so startup failures can be diagnosed.
