import { invoke } from "@tauri-apps/api/core";

const usernameEl = document.getElementById("username-display") as HTMLSpanElement;
const passwordEl = document.getElementById("password") as HTMLInputElement;
const toggleShowBtn = document.getElementById("toggle-show") as HTMLButtonElement;
const startBtn = document.getElementById("start") as HTMLButtonElement;
const statusEl = document.getElementById("status") as HTMLSpanElement;

const stepPwd = document.getElementById("step-password") as HTMLElement;
const step2fa = document.getElementById("step-2fa") as HTMLElement;
const codeEl = document.getElementById("code") as HTMLInputElement;
const submitBtn = document.getElementById("submit") as HTMLButtonElement;
const status2faEl = document.getElementById("status-2fa") as HTMLSpanElement;

type LoginResult = {
  needs_2fa?: boolean;
  needs_password?: boolean;
  error?: string;
  success?: boolean;
};

type SubmitResult = {
  success?: boolean;
  error?: string;
  warning?: string;
  note?: string;
};

function setStatus(
  el: HTMLSpanElement,
  text: string,
  kind: "ok" | "err" | "info" | "none" = "none",
) {
  el.textContent = text;
  el.classList.remove("ok", "err", "info");
  if (kind !== "none") el.classList.add(kind);
}

async function init() {
  const username = await invoke<string>("icloud_auth_username");
  usernameEl.textContent = username.trim() || "(ICLOUD_USERNAME in .env nicht gesetzt)";
}

toggleShowBtn.addEventListener("click", () => {
  if (passwordEl.type === "password") {
    passwordEl.type = "text";
    toggleShowBtn.textContent = "Verbergen";
  } else {
    passwordEl.type = "password";
    toggleShowBtn.textContent = "Anzeigen";
  }
});

startBtn.addEventListener("click", async () => {
  startBtn.disabled = true;
  setStatus(statusEl, "Spreche mit Apple …", "info");
  try {
    const res = await invoke<LoginResult>("icloud_auth_login", {
      password: passwordEl.value,
    });
    if (res.error) {
      setStatus(statusEl, res.error, "err");
      startBtn.disabled = false;
      return;
    }
    if (res.success && !res.needs_2fa) {
      // Already trusted — nothing more to do.
      setStatus(statusEl, "iCloud-Trust ist noch gültig — nichts zu tun.", "ok");
      startBtn.disabled = false;
      passwordEl.value = "";
      return;
    }
    if (res.needs_2fa) {
      setStatus(statusEl, "Code wurde an deine Apple-Geräte gepusht.", "info");
      stepPwd.hidden = true;
      step2fa.hidden = false;
      passwordEl.value = "";
      codeEl.focus();
      return;
    }
    setStatus(statusEl, "Unerwartete Antwort vom Helper.", "err");
    startBtn.disabled = false;
  } catch (err) {
    setStatus(statusEl, `Fehler: ${err}`, "err");
    startBtn.disabled = false;
  }
});

submitBtn.addEventListener("click", async () => {
  const code = codeEl.value.trim();
  if (!/^\d{6}$/.test(code)) {
    setStatus(status2faEl, "Code muss 6 Ziffern haben.", "err");
    return;
  }
  submitBtn.disabled = true;
  setStatus(status2faEl, "Verifiziere …", "info");
  try {
    const res = await invoke<SubmitResult>("icloud_auth_submit_2fa", { code });
    if (res.error) {
      setStatus(status2faEl, res.error, "err");
      submitBtn.disabled = false;
      return;
    }
    if (res.success) {
      const extra = res.warning ? ` (${res.warning})` : "";
      setStatus(
        status2faEl,
        `Verbunden${extra} — Fibi kann iCloud-Reminders wieder lesen.`,
        "ok",
      );
      codeEl.value = "";
      return;
    }
    setStatus(status2faEl, "Unerwartete Antwort.", "err");
    submitBtn.disabled = false;
  } catch (err) {
    setStatus(status2faEl, `Fehler: ${err}`, "err");
    submitBtn.disabled = false;
  }
});

codeEl.addEventListener("keydown", (e) => {
  if (e.key === "Enter") submitBtn.click();
});

void init();
