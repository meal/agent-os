#!/bin/sh
set -eu
docker compose config --format json | docker compose run --rm -T test pyenv exec python -c '
import json, sys
services = json.load(sys.stdin)["services"]
ui = services["ui"]
assert ui["network_mode"] == "host"
assert not ui.get("ports") and not ui.get("devices") and not ui.get("cap_add")
mounts = {m["target"]: m for m in ui["volumes"]}
assert mounts["/work"]["read_only"] is True
assert mounts["/work/target"].get("read_only", False) is False
assert mounts["/data"]["type"] == "volume"
assert ui["command"][-3:] == ["ui", "--port", "8080"]
assert services["test-ui"]["network_mode"] == "none"
assert not services["test-ui"].get("devices") and not services["test-ui"].get("ports")
assert services["test"]["build"]["target"] == "development"
assert not services["test"].get("devices")
print("UI Compose mounts, networking and privilege boundaries verified")
'
