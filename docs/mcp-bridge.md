# WordCraft, PdfCraft and GridCraft MCP over HTTP

`wordcraft-cli mcp` speaks MCP over stdio (see [mcp.md](mcp.md)). Clients that connect over HTTP
instead, such as Gemini Scribe in Obsidian, reach it through the bridge in
`packaging/mcp-bridge/`: a single Python file (standard library only) that serves each Craft app's
stdio server over MCP's Streamable HTTP transport, one endpoint per app.

| Endpoint | App |
|---|---|
| `http://SERVER:8090/mcp` | GridCraft (spreadsheets) |
| `http://SERVER:8090/word/mcp` | WordCraft (documents) |
| `http://SERVER:8090/pdf/mcp` | PdfCraft (PDFs) |

Every MCP session (each client connection, by its `Mcp-Session-Id`) gets its own app process, so
clients don't edit each other's open document. Sessions left unused for 30 minutes are closed.

## Install on a Linux server

1. **wordcraft-cli.** Download `wordcraft-cli-x86_64-linux.tar.gz` from the `cli-linux-latest`
   release and put the binary where the bridge expects it:

   ```sh
   curl -LO https://github.com/Azurehawke/wordcraft/releases/download/cli-linux-latest/wordcraft-cli-x86_64-linux.tar.gz
   tar xzf wordcraft-cli-x86_64-linux.tar.gz
   sudo install -D -m 755 wordcraft-cli/wordcraft-cli /opt/wordcraft/bin/wordcraft-cli
   /opt/wordcraft/bin/wordcraft-cli --version
   ```

   **pdfcraft-cli** (for `/pdf/mcp`) comes from the `cli-linux-latest` release of
   `Azurehawke/pdfcraft`, with its OCR models in `models/` beside it:

   ```sh
   curl -LO https://github.com/Azurehawke/pdfcraft/releases/download/cli-linux-latest/pdfcraft-cli-x86_64-linux.tar.gz
   tar xzf pdfcraft-cli-x86_64-linux.tar.gz
   sudo mkdir -p /opt/pdfcraft && sudo cp -r pdfcraft-cli/pdfcraft-cli pdfcraft-cli/models /opt/pdfcraft/
   /opt/pdfcraft/pdfcraft-cli --version
   ```

   An app you don't install just answers 503 at its endpoint; the others work.

2. **The bridge.** Replace the GridCraft-only bridge with `craft_mcp_bridge.py` (same port, same
   `/mcp` endpoint and the same `GRIDCRAFT_*` settings, so existing GridCraft clients don't
   change). If it runs as a service, tell it the documents folder, e.g. in the unit:

   ```ini
   Environment=WORDCRAFT_DOCS=/srv/sync/ObsidianVault
   ```

   `packaging/mcp-bridge/craft-mcp-bridge.service` and `craft-mcp-bridge.env` are a complete
   example. Then `sudo systemctl restart <service>` and check the log: it lists each endpoint as
   available or says why not.

3. **The client.** In Gemini Scribe add a second MCP server: transport *HTTP (remote server)*,
   URL `http://SERVER:8090/word/mcp`, then *Test connection*. It should discover `list_commands`,
   `execute`, `batch`, `new_document`, `open_document`, `save_document`, `type_text`,
   `select_text`, `get_text`, `inspect_document`, `render_page` and `parity`. For PDFs add a third
   server with `http://SERVER:8090/pdf/mcp`: it discovers `command_list`, `command_run`,
   `command_batch`, `doc_inspect`, `render_preview` and PdfCraft's task tools (`doc_open`,
   `doc_save`, `text_extract`, `annot_add`, `form_fill`, `ocr_run`...).

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `CRAFT_MCP_HOST` / `GRIDCRAFT_HOST` | `0.0.0.0` | Address to listen on |
| `CRAFT_MCP_PORT` / `GRIDCRAFT_PORT` | `8090` | Port |
| `GRIDCRAFT_BINARY` | `/opt/gridcraft/bin/gridcraft-cli` | GridCraft's CLI |
| `WORDCRAFT_BINARY` | `/opt/wordcraft/bin/wordcraft-cli` | WordCraft's CLI |
| `WORDCRAFT_DOCS` | (none) | WordCraft's folder: relative paths resolve against it, and WordCraft reads and writes only inside it (`--automation-read-root` / `--automation-write-root`). Without it, relative paths resolve against the bridge's folder and nothing is confined. |
| `PDFCRAFT_BINARY` | `/opt/pdfcraft/bin/pdfcraft-cli` | PdfCraft's CLI (keep `models/` beside it for OCR) |
| `PDFCRAFT_DOCS` | `WORDCRAFT_DOCS` | PdfCraft's folder (`--root`): relative paths resolve against it, and PdfCraft reads and writes only inside it |
| `WORDCRAFT_AUTHOR` | `Gemini (Obsidian)` | Name on tracked changes and comments the agent makes |
| `CRAFT_MCP_TIMEOUT` | `120` | Seconds to wait for an app's answer |
| `CRAFT_MCP_IDLE_MINUTES` | `30` | Close sessions unused this long |

## Paths

The agent works on the server's files. With `WORDCRAFT_DOCS` set to the server's copy of the
vault (a Syncthing folder, say), the paths it sees in Obsidian are the paths to use:
`open_document {"path": "Projects/Report.docx"}`, `save_document {"path": "Projects/Report.docx"}`.
Saved files reach the other devices the way the rest of the vault does.

## Security

The bridge has no login of its own, like the GridCraft one it replaces: anyone who can reach the
port can use the apps. Keep it reachable only over Tailscale (or your LAN), not forwarded from
the internet; `WORDCRAFT_DOCS` and `PDFCRAFT_DOCS` keep the apps' reads and writes inside one folder.
