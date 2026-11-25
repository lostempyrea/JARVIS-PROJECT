# JARVIS-PROJECT
Mom i want a jarvis
But we have a jarvis at home

## Architecture
- **jarvis-core**: Core engine handling cross-platform integrations, channels (Telegram, Discord, Voice/Groq), state management, and plugin architecture.
- **jarvis-app**: Modern Dioxus/Tauri desktop application featuring a Win32 ConPTY multi-tab terminal, settings HUD, and chat interface.

## Configuration & Secrets
- Secrets and API keys are securely stored locally in the Windows Credential Manager via the `keyring` crate.
- Runtime configuration is stored in `%APPDATA%\Jarvis\config.toml`.

## Building
To build the release version of the application:
```bash
cargo build --release -p jarvis-app
```
