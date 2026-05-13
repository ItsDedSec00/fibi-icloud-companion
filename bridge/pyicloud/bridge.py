"""Pyicloud bridge sidecar for Companion.

Reads newline-delimited JSON requests from stdin, writes NDJSON responses
to stdout, sends progress/error logs to stderr.

Request:  {"id": "<any-id>", "op": "<op-name>", "args": {...}}
Response: {"id": "<echo>", "result": <result>}   on success
          {"id": "<echo>", "error":  "<msg>"}     on failure

Operations:
  - list_lists                       → [{id, title, count}]
  - list_reminders {list?, only_open?} → [Reminder]
  - create_reminder {title, list?, due_iso?, notes?} → {success, message, id}
  - complete_reminder {id}           → {success}
  - delete_reminder   {id}           → {success}

Auth: reads ICLOUD_USERNAME from env, fetches password from Windows
Credential Manager (must have been seeded by auth_setup.py first).
On 2FA-required state (trust expired), returns error 'needs_reauth' on
every op so the Rust side can surface a UI.

CloudKit's TRY_AGAIN_LATER (HTTP 400 with retryAfter while a secondary
index is being built) is handled transparently with exponential backoff.
"""

from __future__ import annotations

import json
import os
import re
import sys
import time
import traceback
from datetime import datetime
from typing import Any, Callable

# Force UTF-8 on all stdio. Default on Windows is cp1252 which can't encode
# the umlauts in users' reminder titles. Must happen before any I/O.
for _stream in (sys.stdout, sys.stderr, sys.stdin):
    try:
        _stream.reconfigure(encoding="utf-8", newline="\n")  # type: ignore[union-attr]
    except Exception:
        pass

from pyicloud import PyiCloudService
from pyicloud import utils as pyu  # type: ignore
from pyicloud.exceptions import PyiCloudAPIResponseException

MAX_INDEX_RETRIES = 6
DEFAULT_BACKOFF_S = 35

# Lazily-initialised on first request. Reused across ops in the same process.
_api: PyiCloudService | None = None


# ── Auth / API session ───────────────────────────────────────────────────

def get_api() -> PyiCloudService:
    """Return a logged-in PyiCloudService. Re-uses session if available."""
    global _api
    username = (os.environ.get("ICLOUD_USERNAME") or "").strip()
    if not username:
        raise RuntimeError("ICLOUD_USERNAME ist nicht gesetzt.")
    if _api is None:
        password = pyu.get_password_from_keyring(username)
        if not password:
            raise RuntimeError(
                "needs_reauth: kein Passwort im Credential Manager — "
                "lauf bridge/pyicloud/auth_setup.py."
            )
        try:
            _api = PyiCloudService(username, password)
        except Exception as e:  # noqa: BLE001
            raise RuntimeError(f"Auth fehlgeschlagen: {e}") from e
    if _api.requires_2fa:
        # Trust cookie abgelaufen. Companion soll User-Prompt machen.
        raise RuntimeError(
            "needs_reauth: 2FA verlangt — Trust ist abgelaufen, "
            "lauf bridge/pyicloud/auth_setup.py."
        )
    return _api


# ── CloudKit retry helper ────────────────────────────────────────────────

def with_index_retry(fn: Callable[[], Any]) -> Any:
    """Run `fn` and auto-retry on Apple's TRY_AGAIN_LATER."""
    last_exc: Exception | None = None
    for attempt in range(1, MAX_INDEX_RETRIES + 1):
        try:
            return fn()
        except PyiCloudAPIResponseException as e:
            msg = str(e)
            if "TRY_AGAIN_LATER" not in msg and "retryAfter" not in msg:
                raise
            wait = parse_retry_after(msg) or DEFAULT_BACKOFF_S
            last_exc = e
            print(
                f"[bridge] TRY_AGAIN_LATER {attempt}/{MAX_INDEX_RETRIES}, "
                f"warte {wait}s",
                file=sys.stderr,
                flush=True,
            )
            time.sleep(wait + 2)
    assert last_exc is not None
    raise last_exc


def parse_retry_after(msg: str) -> int | None:
    m = re.search(r'"retryAfter"\s*:\s*(\d+)', msg)
    return int(m.group(1)) if m else None


# ── List matching (name → list_id) ───────────────────────────────────────

def match_lists(lists: list, needle: str) -> list:
    """Find lists whose ID matches exactly OR whose title contains the
    needle (case-insensitive). Used for both bridge filter arg and the
    optional whitelist."""
    n = needle.strip().lower()
    if not n:
        return list(lists)
    # Exact ID match first (e.g. 'List/F3382...' or just the UUID).
    by_id = [l for l in lists if n in str(_attr(l, "id", "identifier", "guid", default="")).lower()]
    if by_id:
        return by_id
    # Then title prefix / substring.
    return [
        l for l in lists
        if str(_attr(l, "title", "name", default="")).lower().startswith(n)
        or n in str(_attr(l, "title", "name", default="")).lower()
    ]


def parse_whitelist() -> list[str] | None:
    raw = os.environ.get("ICLOUD_REMINDER_LISTS", "").strip()
    if not raw:
        return None
    return [s.strip() for s in raw.split(",") if s.strip()]


def apply_whitelist(lists: list) -> list:
    wl = parse_whitelist()
    if not wl:
        return list(lists)
    keep: list = []
    for entry in wl:
        for hit in match_lists(lists, entry):
            if hit not in keep:
                keep.append(hit)
    if not keep:
        # Fail-open: empty whitelist match → return all so user sees data.
        print(
            "[bridge] ICLOUD_REMINDER_LISTS matched nothing — using all lists",
            file=sys.stderr,
            flush=True,
        )
        return list(lists)
    return keep


# ── Helpers ──────────────────────────────────────────────────────────────

def _attr(obj: Any, *names: str, default: Any = None) -> Any:
    for n in names:
        if hasattr(obj, n):
            v = getattr(obj, n)
            if v is not None:
                return v
        if isinstance(obj, dict) and obj.get(n) is not None:
            return obj[n]
    return default


def _fmt_dt(v: Any) -> str | None:
    if v is None:
        return None
    if hasattr(v, "isoformat"):
        return v.isoformat()
    return str(v)


def _parse_iso(v: str | None) -> datetime | None:
    if not v:
        return None
    s = v.strip()
    if not s:
        return None
    # Accept '...Z' suffix.
    if s.endswith("Z"):
        s = s[:-1] + "+00:00"
    return datetime.fromisoformat(s)


# ── Operations ───────────────────────────────────────────────────────────

def op_list_lists(_args: dict) -> Any:
    rem = get_api().reminders
    lists = apply_whitelist(list(rem.lists()))
    return [
        {
            "id": str(_attr(l, "id", "identifier", "guid", default="")),
            "title": _attr(l, "title", "name", default=""),
            "count": _attr(l, "count", "size"),
        }
        for l in lists
    ]


def op_list_reminders(args: dict) -> Any:
    rem = get_api().reminders
    list_filter = (args.get("list") or "").strip()
    only_open = bool(args.get("only_open", True))

    all_lists = apply_whitelist(list(rem.lists()))
    if list_filter:
        target = match_lists(all_lists, list_filter)
        if not target:
            return {"error": f"Keine Reminder-Liste matcht '{list_filter}'"}
    else:
        target = all_lists

    out: list[dict] = []
    for lst in target:
        list_id = str(_attr(lst, "id", "identifier", "guid", default=""))
        list_title = _attr(lst, "title", "name", default="?")
        try:
            reminders = with_index_retry(lambda lid=list_id: list(rem.reminders(list_id=lid)))
        except Exception as e:  # noqa: BLE001
            print(f"[bridge] Liste {list_title} übersprungen: {e}", file=sys.stderr, flush=True)
            continue
        for r in reminders:
            done = bool(_attr(r, "completed", "is_completed", default=False))
            if only_open and done:
                continue
            out.append({
                "id": str(_attr(r, "id", "identifier", "guid", default="")),
                "title": _attr(r, "title", "summary", default=""),
                "due": _fmt_dt(_attr(r, "due", "due_date", "due_at")),
                "completed": done,
                "notes": _attr(r, "description", "notes", "desc"),
                "list": list_title,
            })

    # Open first, then by due (None last), then by title.
    out.sort(key=lambda x: (
        x["completed"],
        x["due"] or "9999-12-31T00:00:00",
        x["title"] or "",
    ))
    return out


def op_create_reminder(args: dict) -> Any:
    if not args.get("title"):
        return {"error": "title fehlt"}
    title = str(args["title"]).strip()
    list_filter = (args.get("list") or "").strip()
    due_iso = args.get("due_iso")
    notes = args.get("notes")

    rem = get_api().reminders
    all_lists = apply_whitelist(list(rem.lists()))
    target = None
    if list_filter:
        matches = match_lists(all_lists, list_filter)
        if not matches:
            return {"error": f"Keine Reminder-Liste matcht '{list_filter}'"}
        target = matches[0]
    else:
        # Default via env var, else first whitelisted list.
        default = (os.environ.get("ICLOUD_DEFAULT_WRITE_REMINDER_LIST") or "").strip()
        if default:
            matches = match_lists(all_lists, default)
            if matches:
                target = matches[0]
        if target is None:
            if not all_lists:
                return {"error": "Keine Reminder-Liste gefunden."}
            target = all_lists[0]

    due_dt = _parse_iso(due_iso) if isinstance(due_iso, str) else None
    list_title = _attr(target, "title", "name", default="?")
    list_id = str(_attr(target, "id", "identifier", "guid", default=""))

    try:
        created = rem.create(
            list_id=list_id,
            title=title,
            desc=notes if isinstance(notes, str) and notes.strip() else None,
            due_date=due_dt,
        )
    except TypeError:
        # pyicloud API churn: some versions name kwargs differently.
        # Fall back to positional args (title, list_id, ...).
        created = rem.create(title, list_id, due_dt, notes)  # type: ignore[misc]

    return {
        "success": True,
        "message": f'"{title}" in {list_title} angelegt',
        "id": str(_attr(created, "id", "identifier", default="")),
        "list": list_title,
    }


def op_complete_reminder(args: dict) -> Any:
    """Mark a reminder as completed by id."""
    rid = (args.get("id") or "").strip()
    if not rid:
        return {"error": "id fehlt"}
    rem = get_api().reminders
    target = rem.get(rid)
    if target is None:
        return {"error": f"Reminder {rid} nicht gefunden"}
    setattr(target, "completed", True)
    rem.update(target)
    return {"success": True}


def op_list_contacts(_args: dict) -> Any:
    """All contacts in the user's iCloud address book. pyicloud caches the
    list internally and `all` re-fetches each call — so the first hit
    after process start is slow (whole address book), subsequent are fast.
    We let the Rust side cache aggressively (contacts barely change)."""
    api = get_api()
    raw = api.contacts.all or []
    out = []
    for c in raw:
        first = (c.get("firstName") or "").strip()
        last = (c.get("lastName") or "").strip()
        middle = (c.get("middleName") or "").strip()
        nick = (c.get("nickName") or c.get("nickname") or "").strip()
        company = (c.get("companyName") or "").strip()
        name_parts = [first, middle, last]
        name = " ".join(p for p in name_parts if p).strip() or nick or company or "(ohne Name)"
        emails = []
        for e in c.get("emailAddresses") or []:
            field = (e.get("field") or "").strip()
            if field:
                emails.append({"label": e.get("label"), "value": field})
        phones = []
        for p in c.get("phones") or []:
            field = (p.get("field") or "").strip()
            if field:
                phones.append({"label": p.get("label"), "value": field})
        bday = c.get("birthday")
        if bday and isinstance(bday, dict):
            y = bday.get("year")
            m = bday.get("month")
            d = bday.get("day")
            if m and d:
                bday_str = f"{y}-{int(m):02d}-{int(d):02d}" if y else f"--{int(m):02d}-{int(d):02d}"
            else:
                bday_str = None
        else:
            bday_str = None
        out.append({
            "id": c.get("contactId") or c.get("etag") or name,
            "name": name,
            "first_name": first or None,
            "last_name": last or None,
            "nickname": nick or None,
            "company": company or None,
            "emails": emails,
            "phones": phones,
            "birthday": bday_str,
        })
    return out


def op_delete_reminder(args: dict) -> Any:
    rid = (args.get("id") or "").strip()
    if not rid:
        return {"error": "id fehlt"}
    rem = get_api().reminders
    target = rem.get(rid)
    if target is None:
        return {"error": f"Reminder {rid} nicht gefunden"}
    rem.delete(target)
    return {"success": True}


OPS: dict[str, Callable[[dict], Any]] = {
    "list_lists": op_list_lists,
    "list_reminders": op_list_reminders,
    "create_reminder": op_create_reminder,
    "complete_reminder": op_complete_reminder,
    "delete_reminder": op_delete_reminder,
    "list_contacts": op_list_contacts,
}


# ── Stdin/stdout NDJSON loop ────────────────────────────────────────────

def send(obj: dict) -> None:
    sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def main() -> int:
    print("[bridge] ready", file=sys.stderr, flush=True)
    for raw in sys.stdin:
        line = raw.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except Exception as e:  # noqa: BLE001
            send({"error": f"bad JSON: {e}"})
            continue
        req_id = req.get("id")
        op_name = req.get("op")
        args = req.get("args") or {}
        try:
            handler = OPS.get(op_name)
            if not handler:
                send({"id": req_id, "error": f"unknown op: {op_name}"})
                continue
            result = handler(args)
            if isinstance(result, dict) and "error" in result and "success" not in result:
                send({"id": req_id, "error": result["error"]})
            else:
                send({"id": req_id, "result": result})
        except Exception as e:  # noqa: BLE001
            traceback.print_exc(file=sys.stderr)
            send({"id": req_id, "error": str(e)})
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
