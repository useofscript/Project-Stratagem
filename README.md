# Project Stratagem

Project Stratagem is a Windows desktop control panel for the local Ollama gateway stack.

## Features
- Start Ollama, Node bridge, and Cloudflare Quick Tunnel from one button
- Stop only processes launched by Stratagem (other Windows services are left alone)
- Live status indicators for local ports 11434 and 11435 and tunnel process
- Test Ollama API and list installed models
- Browse/select bridge script, Node.js, and cloudflared paths
- Persist app configuration under `%LOCALAPPDATA%\\ProjectStratagem`
- Encrypt bridge token with Windows DPAPI, scoped to the current Windows account

## Requirements
- Windows 10 or 11
- Ollama
- Node.js
- cloudflared
- The gateway's existing `bridge.mjs` file

## Running from source
Install Python 3.12+ and run:

```powershell
python app.py
```

## Download the Windows EXE
Use the Actions tab and run **Build Project Stratagem for Windows** to build a portable `Project-Stratagem.exe`. Tag pushes matching `v*` also trigger the build. The workflow uploads the EXE as a downloadable artifact.

## First-time configuration
1. Launch Project Stratagem.
2. Browse to the existing `bridge.mjs` if the auto-detected path is incorrect.
3. Confirm the Node and cloudflared paths.
4. Enter the bridge token. A generated token is only useful if the bridge and any gateway/Worker secret are configured with the same token. The environment variable name defaults to `BRIDGE_TOKEN`; change it if your bridge expects another name.
5. Click **Save Token** and **Save Settings**.
6. Click **Start All**. The Cloudflare Quick Tunnel URL is temporary; see `%LOCALAPPDATA%\\ProjectStratagem\\logs\\tunnel.log`.

**Important:** The app starts a Quick Tunnel but does not automatically edit Cloudflare Worker settings or remote gateway configuration. Check the tunnel log and update your Worker endpoint/secret as needed.

## App data and logs
- Settings: `%LOCALAPPDATA%\\ProjectStratagem\\settings.json`
- Token: `%LOCALAPPDATA%\\ProjectStratagem\\token.dpapi` (encrypted with DPAPI)
- Logs: `%LOCALAPPDATA%\\ProjectStratagem\\logs`
