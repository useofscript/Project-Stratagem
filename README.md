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

The app includes a redesigned command center, sidebar navigation, service-health indicators, quick actions, a Clank World connection panel, Settings, and Activity. A chess king icon is used for the app window and Windows executable. The Windows build is a standalone `Project-Stratagem.exe`; Ollama, Node.js, cloudflared, and your existing bridge script remain separate prerequisites.

## Requirements

- Windows 10 or Windows 11 (64-bit)
- [Ollama](https://ollama.com/download)
- [Node.js](https://nodejs.org/)
- [Cloudflare Tunnel / cloudflared](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/)
- Your existing `bridge.mjs` gateway script

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
4. Enter the token configured in the bridge. The app's Show and Copy controls let you retrieve the exact token for a direct Clank World setup.
5. Select **Save encrypted token**, then **Save Settings**.
6. Return to **Dashboard** and select **Start All**.

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
