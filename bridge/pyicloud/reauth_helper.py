"""pyicloud re-auth helper, driven by the Companion Settings window.

Two subcommands, one JSON line per call on stdout:

  python reauth_helper.py trigger_2fa
      - Reads username from ICLOUD_USERNAME env.
      - Reads password from the Windows Credential Manager (keyring service
        "pyicloud", account = username).
      - Creates a PyiCloudService — if Apple wants 2FA, the trusted device
        prompt fires immediately.
      - Output: {"needs_2fa": bool, "needs_password": bool, "error": str?}

  python reauth_helper.py submit_2fa <code>
      - Loads the same user/password, recreates PyiCloudService (pyicloud
        loads its cached session cookies so we're still in the post-login
        / pre-2FA state from the previous call).
      - Calls validate_2fa_code + trust_session.
      - Output: {"success": bool, "error": str?}

Optionally takes a fresh password as an arg for the rare case where the
keyring is empty (first-time setup or after a logout):

  python reauth_helper.py trigger_2fa --password <pwd>
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import traceback

from pyicloud import PyiCloudService
from pyicloud import utils as pyu  # type: ignore


def get_username() -> str:
    u = (os.environ.get("ICLOUD_USERNAME") or "").strip()
    if not u:
        raise RuntimeError("ICLOUD_USERNAME ist nicht gesetzt (siehe .env).")
    return u


def get_password(username: str, override: str | None) -> str:
    if override and override.strip():
        return override.strip()
    pw = pyu.get_password_from_keyring(username)
    if pw:
        return pw
    raise RuntimeError(
        "needs_password: kein Passwort im Credential Manager. Bitte einmal "
        "Apple-ID-Passwort eingeben."
    )


def emit(obj: dict) -> None:
    sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def do_trigger(args: argparse.Namespace) -> int:
    username = get_username()
    try:
        password = get_password(username, args.password)
    except RuntimeError as e:
        msg = str(e)
        emit({"needs_2fa": False, "needs_password": msg.startswith("needs_password"), "error": msg})
        return 0
    try:
        api = PyiCloudService(username, password)
    except Exception as e:  # noqa: BLE001
        emit({"needs_2fa": False, "needs_password": False, "error": f"login: {e}"})
        return 1

    # If the user supplied a fresh password, persist it now so submit_2fa
    # can read it back from keyring without us needing to keep state.
    if args.password and args.password.strip():
        try:
            pyu.store_password_in_keyring(username, args.password.strip())
        except Exception:  # noqa: BLE001
            pass

    if api.requires_2fa:
        emit({"needs_2fa": True, "needs_password": False})
        return 0
    if api.requires_2sa:
        emit({
            "needs_2fa": False,
            "needs_password": False,
            "error": "Apple möchte Legacy-2SA — bitte einmalig auth_setup.py manuell laufen lassen.",
        })
        return 1
    # Already trusted (no 2FA prompt needed) — nothing to do.
    emit({"needs_2fa": False, "needs_password": False, "success": True})
    return 0


def do_submit(args: argparse.Namespace) -> int:
    username = get_username()
    try:
        password = get_password(username, None)
    except RuntimeError as e:
        emit({"success": False, "error": str(e)})
        return 1
    try:
        api = PyiCloudService(username, password)
    except Exception as e:  # noqa: BLE001
        emit({"success": False, "error": f"login: {e}"})
        return 1

    if not api.requires_2fa:
        # Already trusted before we got the code — treat as success.
        emit({"success": True, "note": "already trusted"})
        return 0

    code = args.code.strip()
    try:
        if not api.validate_2fa_code(code):
            emit({"success": False, "error": "2FA-Code abgelehnt."})
            return 1
        if not api.trust_session():
            emit({
                "success": True,
                "warning": "validate ok, aber trust_session fehlgeschlagen — nächste Session braucht wieder 2FA.",
            })
            return 0
        emit({"success": True})
        return 0
    except Exception as e:  # noqa: BLE001
        emit({"success": False, "error": f"validate: {e}"})
        return 1


def main() -> int:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_trigger = sub.add_parser("trigger_2fa")
    p_trigger.add_argument("--password", default=None,
                           help="Optional: frisches Passwort, das im Keyring gespeichert wird")

    p_submit = sub.add_parser("submit_2fa")
    p_submit.add_argument("code", help="6-stelliger 2FA-Code vom Apple-Gerät")

    args = parser.parse_args()
    if args.cmd == "trigger_2fa":
        return do_trigger(args)
    if args.cmd == "submit_2fa":
        return do_submit(args)
    parser.print_help()
    return 2


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as e:  # noqa: BLE001
        traceback.print_exc(file=sys.stderr)
        emit({"error": str(e)})
        sys.exit(99)
