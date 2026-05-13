"""Interactive auth setup + smoke test for pyicloud's Reminders service.

Run this ONCE in a normal cmd / PowerShell window. It prompts for:
  - Apple ID (full email)
  - Apple ID PASSWORD (the real one — NOT the app-specific password from .env)
  - 2FA code (Apple sends it to your trusted devices)

On success:
  - Stores Apple-ID password in the Windows Credential Manager via `keyring`.
  - Trusts this device so future runs don't need 2FA for ~30 days.
  - Caches session cookies in %USERPROFILE%\\.pyicloud\\.
  - Lists your reminder lists + a couple of reminders from each to prove the
    modern CloudKit-based API actually reaches your data.

After this succeeds, the bridge (run non-interactively) reads the stored
password from keyring and reuses the cached session — no further prompts.
"""

from __future__ import annotations

import getpass
import sys
from typing import Any

from pyicloud import PyiCloudService
from pyicloud import utils as pyu  # type: ignore


def main() -> int:
    username = input("Apple-ID (email): ").strip()
    if not username:
        print("Apple-ID leer — Abbruch.", file=sys.stderr)
        return 1

    # Don't echo password.
    password = getpass.getpass("Apple-ID Passwort (NICHT app-specific): ")
    if not password:
        print("Passwort leer — Abbruch.", file=sys.stderr)
        return 1

    print("\n→ Authentifiziere bei iCloud …", flush=True)
    try:
        api = PyiCloudService(username, password)
    except Exception as e:  # noqa: BLE001
        print(f"Auth-Fehler: {e}", file=sys.stderr)
        return 2

    if api.requires_2fa:
        print("\n→ 2FA erforderlich. Apple poppt jetzt einen 6-stelligen Code")
        print("  auf deinen Trusted Devices (iPhone/Mac) auf.")
        # Triggers Apple to push the code. Idempotent in modern pyicloud.
        api.request_2fa_code()
        code = input("2FA-Code (6 Ziffern): ").strip()
        if not api.validate_2fa_code(code):
            print("2FA-Code ungültig.", file=sys.stderr)
            return 3
        print("→ 2FA OK. Aktiviere Device-Trust (spart künftiges 2FA) …")
        if not api.trust_session():
            print(
                "Warnung: trust_session schlug fehl — "
                "du wirst beim nächsten Login wieder 2FA brauchen."
            )

    elif api.requires_2sa:
        print("Account benutzt Legacy-2SA. Auswahl der Trust-Methode:")
        for i, dev in enumerate(api.trusted_devices):
            print(f"  [{i}] {dev.get('deviceName', dev)}")
        sel = int(input("Index: "))
        device = api.trusted_devices[sel]
        if not api.send_verification_code(device):
            print("2SA-Code-Anforderung schlug fehl.", file=sys.stderr)
            return 4
        code = input("2SA-Code: ").strip()
        if not api.validate_verification_code(device, code):
            print("2SA-Code ungültig.", file=sys.stderr)
            return 5

    # Persist password into Windows Credential Manager via keyring so the
    # bridge can run non-interactively in the future.
    try:
        pyu.store_password_in_keyring(username, password)
        print("→ Passwort im Windows Credential Manager gespeichert.")
    except Exception as e:  # noqa: BLE001
        print(f"Warnung: keyring-Speicher schlug fehl ({e}) — bridge wird Passwort erneut brauchen.")

    print("\n=== Reminders Smoke Test ===")
    try:
        rem = api.reminders
    except Exception as e:  # noqa: BLE001
        print(f"Reminders-Service nicht verfügbar: {e}", file=sys.stderr)
        return 6

    try:
        lists = list(rem.lists())  # type: ignore[attr-defined]
    except Exception as e:  # noqa: BLE001
        print(f"Liste der Reminder-Listen fehlgeschlagen: {e}", file=sys.stderr)
        return 7

    print(f"Gefundene Reminder-Listen: {len(lists)}")
    for lst in lists:
        # Different pyicloud versions expose different attrs; print whatever we can find.
        title = _attr(lst, "title", "name", "displayName") or "(no title)"
        list_id = _attr(lst, "id", "identifier", "guid") or "?"
        count = _attr(lst, "count", "size") or "?"
        print(f"  • {title}   (id={list_id}, count={count})")

        # Pull up to 3 reminders from this list.
        try:
            reminders = list(rem.reminders(list_id=list_id))[:3]  # type: ignore[attr-defined]
        except Exception as e:  # noqa: BLE001
            print(f"     [reminders() fehlgeschlagen: {e}]")
            continue
        for r in reminders:
            rt = _attr(r, "title", "summary") or "(no title)"
            due = _attr(r, "due", "due_date", "due_at")
            print(f"     - {rt}    due={due}")

    print("\n✓ Auth + Reminders-API funktionieren. Bridge kann gebaut werden.")
    return 0


def _attr(obj: Any, *names: str) -> Any:
    for n in names:
        if hasattr(obj, n):
            v = getattr(obj, n)
            if v is not None:
                return v
    # Fallback: maybe it's a dict-like.
    if isinstance(obj, dict):
        for n in names:
            if n in obj and obj[n] is not None:
                return obj[n]
    return None


if __name__ == "__main__":
    try:
        code = main()
    except KeyboardInterrupt:
        print("\nAbbruch.")
        code = 130
    except Exception as e:  # noqa: BLE001
        import traceback
        traceback.print_exc()
        code = 99
    print(f"\n[Script beendet mit Exit-Code {code}. Drück Enter zum Schließen.]")
    try:
        input()
    except EOFError:
        pass
    raise SystemExit(code)
