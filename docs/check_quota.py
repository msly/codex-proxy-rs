#!/usr/bin/env python3
import json
import sys
import time
import urllib.error
import urllib.request
from collections import Counter
from pathlib import Path

USAGE_URL = "https://chatgpt.com/backend-api/wham/usage"
USER_AGENT = (
    "OpenAI/ChatGPT Codex/1.2025.070.01 "
    "(Linux; x86_64; rv:134.0) Gecko/20100101 Firefox/134.0"
)


def load_tokens(auth_dir: Path):
    rows = []
    for path in sorted(auth_dir.glob("*.json")):
        with path.open("r", encoding="utf-8") as f:
            data = json.load(f)
        rows.append((path, data))
    return rows


def fetch_quota(data):
    headers = {
        "Authorization": "Bearer " + data.get("access_token", ""),
        "Content-Type": "application/json",
        "User-Agent": USER_AGENT,
        "Origin": "https://chatgpt.com",
        "Referer": "https://chatgpt.com/",
    }
    if data.get("account_id"):
        headers["Chatgpt-Account-Id"] = data["account_id"]

    req = urllib.request.Request(USAGE_URL, headers=headers, method="GET")
    status = None
    body = b""
    error = ""

    try:
        with urllib.request.urlopen(req, timeout=20) as resp:
            status = resp.status
            body = resp.read()
    except urllib.error.HTTPError as exc:
        status = exc.code
        body = exc.read()
    except Exception as exc:  # noqa: BLE001
        error = f"{type(exc).__name__}: {exc}"

    used_percent = None
    code = ""
    message = ""
    if body:
        try:
            payload = json.loads(body.decode("utf-8"))
            used_percent = (
                payload.get("rate_limit", {})
                .get("primary_window", {})
                .get("used_percent")
            )
            code = (payload.get("error") or {}).get("code", "")
            message = (payload.get("error") or {}).get("message", "")
        except Exception:  # noqa: BLE001
            message = body.decode("utf-8", "replace")[:200]

    return {
        "status": status,
        "used_percent": used_percent,
        "detail": code or error or message,
    }


def sort_key(item):
    if item["used_percent"] is None:
        return (1, 10_000, item["email"])
    return (0, float(item["used_percent"]), item["email"])


def main():
    base_dir = Path(__file__).resolve().parent
    auth_dir = base_dir / "auths"
    tokens = load_tokens(auth_dir)

    if not tokens:
        print(f"no token files found in {auth_dir}", file=sys.stderr)
        return 1

    results = []
    for path, data in tokens:
        result = fetch_quota(data)
        results.append(
            {
                "file": path.name,
                "email": data.get("email", ""),
                "status": result["status"],
                "used_percent": result["used_percent"],
                "detail": result["detail"],
            }
        )
        time.sleep(0.2)

    results.sort(key=sort_key)

    status_counts = Counter(item["status"] for item in results)
    print(f"tokens={len(results)} status_counts={dict(status_counts)}")
    print("email\tstatus\tused_percent\tdetail\tfile")
    for item in results:
        used = (
            ""
            if item["used_percent"] is None
            else f"{float(item['used_percent']):.2f}"
        )
        print(
            "\t".join(
                [
                    item["email"],
                    str(item["status"]),
                    used,
                    item["detail"][:120],
                    item["file"],
                ]
            )
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
