"""Credentials / settings persistence."""
from __future__ import annotations

import getpass
import json
import os
from pathlib import Path

CONFIG_DIR = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "soulsucker"
CONFIG_FILE = CONFIG_DIR / "config.json"


def load_config() -> dict:
    try:
        return json.loads(CONFIG_FILE.read_text())
    except (OSError, ValueError):
        return {}


def save_config(data: dict) -> None:
    CONFIG_DIR.mkdir(parents=True, exist_ok=True)
    tmp = CONFIG_FILE.with_suffix(".tmp")
    tmp.write_text(json.dumps(data, indent=2))
    tmp.replace(CONFIG_FILE)


def resolve_credentials(args_user: str | None, args_pass: str | None) -> tuple[str, str]:
    cfg = load_config()
    user = (
        args_user
        or os.environ.get("SLSK_USERNAME")
        or os.environ.get("SOULSUCKER_USER")
        or os.environ.get("SLSK_USER")
        or cfg.get("username", "")
    )
    password = (
        args_pass
        or os.environ.get("SLSK_PASSWORD")
        or os.environ.get("SOULSUCKER_PASS")
        or cfg.get("password", "")
    )
    if not user:
        user = input("Soulseek username: ").strip()
    if not password:
        password = getpass.getpass("Soulseek password: ")
    if not user:
        raise SystemExit("error: Soulseek username is required")
    # Offer to remember (only if not already stored).
    if cfg.get("username") != user or cfg.get("password") != password:
        try:
            ans = input("Remember credentials in ~/.config/soulsucker/config.json? [y/N]: ").strip().lower()
        except EOFError:
            ans = "n"
        if ans in ("y", "yes"):
            cfg["username"] = user
            cfg["password"] = password
            save_config(cfg)
    return user, password


def resolve_acoustid_key(args_key: str | None) -> str:
    """Free key from https://acoustid.org/api. Empty string = check disabled."""
    if args_key:
        return args_key
    cfg = load_config()
    return (
        os.environ.get("ACOUSTID_API_KEY")
        or os.environ.get("SOULSUCKER_ACOUSTID_KEY")
        or cfg.get("acoustid_key", "")
        or cfg.get("acoustid_api_key", "")
        or ""
    )
