#!/usr/bin/env python3
"""Package a private plugin after verifying the real remote MCP endpoint."""
import argparse
import json
from pathlib import Path
from urllib.parse import urlsplit
from zipfile import ZIP_DEFLATED, ZipFile


def build(endpoint, client_id, output):
    parsed = urlsplit(endpoint)
    if parsed.scheme != "https" or not parsed.hostname or parsed.username or parsed.password or parsed.path != "/mcp" or parsed.query or parsed.fragment:
        raise ValueError("Use the verified HTTPS endpoint ending in /mcp, without credentials or query.")
    if not client_id.strip():
        raise ValueError("Provide the actual OAuth client ID registered with your identity provider.")
    root = Path(__file__).resolve().parents[1] / "integrations/chatgpt"
    manifest = json.loads((root / "plugin.json").read_text())
    assert len(manifest["extensions"]["com.openai"]["interface"]["shortDescription"]) <= 30
    connection = {
        "$schema": "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
        "mcpServers": {"obsidian-vault": {
            "type": "streamable-http", "url": endpoint,
            "extensions": {"com.openai": {"auth": {
                "type": "oauth", "client": {"mode": "provided", "clientId": client_id,
                                               "tokenEndpointAuthMethod": "none"},
                "baseScopes": ["vault:read"], "defaultScopes": ["vault:read", "vault:write"]
            }}}
        }}
    }
    output = Path(output).resolve()
    if output.is_relative_to(root):
        raise ValueError("The output archive must be outside the template directory.")
    with ZipFile(output, "w", ZIP_DEFLATED) as archive:
        for path in sorted(root.rglob("*")):
            if path.is_file():
                archive.write(path, str(Path(manifest["name"]) / path.relative_to(root)))
        archive.writestr(manifest["name"] + "/mcp.json", json.dumps(connection, indent=2)+"\n")
    with ZipFile(output) as archive:
        assert archive.testzip() is None
        for path in archive.namelist():
            if path.endswith(".json"):
                json.loads(archive.read(path))
    return output


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("endpoint")
    parser.add_argument("--client-id", required=True)
    parser.add_argument("--output", default="personal-obsidian-vault.zip")
    args = parser.parse_args()
    print(build(args.endpoint, args.client_id, args.output))
