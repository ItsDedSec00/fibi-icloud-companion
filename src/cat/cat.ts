import { listen } from "@tauri-apps/api/event";
import { ipc, type ChatToken, type SpriteRect } from "../shared/ipc";

// ── Animation state machine ────────────────────────────────────────────────

type Anim =
  | "idle1"
  | "idle2"
  | "lick1"
  | "lick2"
  | "run"
  | "run-long"
  | "sleep"
  | "jump"
  | "humpback";

type Facing = "left" | "right";

interface AnimSpec {
  durationMs: number;
  loops: boolean;
  speedPx: number;
  minDwellMs: number;
  maxDwellMs: number;
}

const ANIM: Record<Anim, AnimSpec> = {
  idle1:    { durationMs: 0,    loops: true,  speedPx: 0,    minDwellMs: 2500, maxDwellMs: 6000 },
  idle2:    { durationMs: 0,    loops: true,  speedPx: 0,    minDwellMs: 2500, maxDwellMs: 6000 },
  lick1:    { durationMs: 0,    loops: true,  speedPx: 0,    minDwellMs: 2000, maxDwellMs: 5000 },
  lick2:    { durationMs: 0,    loops: true,  speedPx: 0,    minDwellMs: 2000, maxDwellMs: 5000 },
  run:      { durationMs: 0,    loops: true,  speedPx: 2.4,  minDwellMs: 1500, maxDwellMs: 4000 },
  "run-long": { durationMs: 0,  loops: true,  speedPx: 3.6,  minDwellMs: 1500, maxDwellMs: 3500 },
  sleep:    { durationMs: 0,    loops: true,  speedPx: 0,    minDwellMs: 8000, maxDwellMs: 20_000 },
  jump:     { durationMs: 700,  loops: false, speedPx: 0,    minDwellMs: 0,    maxDwellMs: 0 },
  humpback: { durationMs: 900,  loops: false, speedPx: 0,    minDwellMs: 0,    maxDwellMs: 0 },
};

const HUMPBACK_COOLDOWN_MS = 8000;
const HUMPBACK_RANDOM_CHANCE = 0.03;
const SPRITE_PATH = "/sprites/cat.png";

// Activity model thresholds. These describe how an *instantaneous* idle
// reading (seconds since last keystroke or foreground change) maps to a
// "target activity level" between 0 (very idle) and 1 (very active). The
// actual `activeLevel` we use for behavior weights is a smoothed blend
// towards this target — so handing the user a keyboard for one second
// won't snap the cat back to "asleep" mode.
const ACTIVE_THRESHOLD = 10;        // ≤ this many idle seconds → fully active
const FULLY_IDLE_THRESHOLD = 60;    // ≥ this many idle seconds → fully idle
// Asymmetric blend: recovering to *active* needs to be fast (user is back
// at the keyboard, the cat should head for the clock immediately), while
// drifting toward *idle* stays gradual ("anteilig erhöhen").
const ACTIVE_BLEND_UP = 0.55;
const ACTIVE_BLEND_DOWN = 0.12;
// Threshold over which we consider the user to have come back, which
// resets the "woke up during idle" flag so the cat can re-enter sleep mode.
const RESET_ACTIVE_THRESHOLD = 0.55;
// Once the cat picks a non-sleep behavior while idle, this flag locks
// sleep weight to 0 % and lets idle ↔ move evolve via `wokeProgress`.
const WOKE_PICK_ACTIVE_MAX = 0.5;
// Each pickNextBehavior that picks idle/move while wokenUp adds this much
// to `wokeProgress`, which biases future picks toward `move`.
const WOKE_PROGRESS_STEP = 0.06;

const catEl = document.getElementById("cat") as HTMLElement;

let anim: Anim = "idle1";
let facing: Facing = "right";
let targetX = 40;
let currentX = 40;
let stateUntil = performance.now() + 4000;
let lastHumpback = 0;
let oneShotEndsAt: number | null = null;
let locked = false;
let pendingSleep = false;
let userIdleSecs = 0;
let sleepX: number | null = null;
let lastPick = "—";
// Frantic mode: when a reminder toast goes 60 s unacknowledged, the cat
// starts sprinting between far ends of the screen to demand attention.
let franticMode = false;

// Smoothed activity level (0 = very idle, 1 = very active). Starts active —
// we assume the user just launched the app and is at their keyboard.
let activeLevel = 1.0;
// Set when the cat picks idle/move during an idle session. While true,
// sleep weight is locked to 0 % until the user comes back to the keyboard.
let wokenUp = false;
// Accumulates while `wokenUp` is true. Drives the idle → move share so the
// cat gradually does more roaming the longer the wake session lasts.
let wokeProgress = 0;

function setAnim(next: Anim) {
  if (anim === next) return;
  catEl.classList.remove(`anim-${anim}`);
  catEl.classList.add(`anim-${next}`);
  anim = next;
}

function setFacing(next: Facing) {
  if (facing === next) return;
  catEl.classList.remove(`facing-${facing}`);
  catEl.classList.add(`facing-${next}`);
  facing = next;
}

function pickIdle(): Anim {
  // 65 % idle (alternating), 35 % lick (alternating).
  if (Math.random() < 0.65) return Math.random() < 0.5 ? "idle1" : "idle2";
  return Math.random() < 0.5 ? "lick1" : "lick2";
}

function pickMovement(): Anim {
  return Math.random() < 0.8 ? "run" : "run-long";
}

interface BehaviorWeights {
  sleep: number;
  idle: number;
  move: number;
}

/** Maps the raw idle-seconds reading to a target activity level in [0, 1].
 *  This target is what `activeLevel` blends toward on each poll — instant
 *  keyboard activity doesn't reset weights to defaults, it just changes
 *  the target. */
function targetActiveLevel(idleSecs: number): number {
  if (idleSecs <= ACTIVE_THRESHOLD) return 1;
  if (idleSecs >= FULLY_IDLE_THRESHOLD) return 0;
  return 1 - (idleSecs - ACTIVE_THRESHOLD) / (FULLY_IDLE_THRESHOLD - ACTIVE_THRESHOLD);
}

/**
 * Two-mode behavior weights:
 *
 *   Asleep mode (`wokenUp = false`):
 *     Linear interpolation by activeLevel.
 *       activeLevel = 1 (user active):  sleep 92 %, idle  8 %, move  0 %
 *       activeLevel = 0 (user idle):    sleep 30 %, idle 50 %, move 20 %
 *     The cat sleeps almost continuously while the user is working, with
 *     just a few stretches/yawns; as the user goes idle the picker mixes
 *     in more idle and a little movement.
 *
 *   Woken mode (`wokenUp = true`):
 *     sleep = 0; move grows with `wokeProgress`, idle fills the rest.
 *     The user coming back lifts `activeLevel` past `RESET_ACTIVE_THRESHOLD`,
 *     which drops `wokenUp` and returns the picker to "asleep mode" so
 *     the cat heads back to the clock.
 */
function behaviorWeights(): BehaviorWeights {
  if (wokenUp) {
    const move = Math.max(0, Math.min(0.7, wokeProgress));
    return { sleep: 0, idle: 1 - move, move };
  }
  const a = activeLevel;
  const sleep = 0.3 + 0.62 * a;
  const idle = 0.5 - 0.42 * a;
  const move = 0.2 - 0.2 * a;
  return { sleep, idle, move };
}

function pickNextBehavior() {
  if (locked) {
    stateUntil = performance.now() + 60_000;
    lastPick = "locked (bubble open)";
    // While locked, the cat should always settle into idle. Without this
    // an in-flight oneShot (e.g. wake-up humpback when the user opens the
    // bubble on a sleeping cat) would end and leave the cat stuck on the
    // animation's last frame.
    if (anim !== "idle1" && anim !== "idle2") {
      transitionTo("idle1");
    }
    return;
  }

  const now = performance.now();

  // Humpback is only acceptable when the cat is already in "woken" mode —
  // we don't want a playful pounce in the user's face while they're typing.
  if (
    wokenUp &&
    Math.random() < HUMPBACK_RANDOM_CHANCE &&
    now - lastHumpback > HUMPBACK_COOLDOWN_MS
  ) {
    lastPick = "humpback (rare)";
    triggerHumpback();
    onCatWakeAction();
    return;
  }

  const w = behaviorWeights();
  const roll = Math.random();
  if (roll < w.sleep) {
    lastPick = `sleep · roll ${roll.toFixed(2)}`;
    goToSleep();
  } else {
    // Picker chose a non-sleep behavior. If we were asleep, do a wake-up
    // humpback (stretch) first. The next pick after the humpback oneShot
    // ends will fall through to idle/move naturally. NO cooldown gate —
    // waking up is a distinct event and should always animate, regardless
    // of how recently any other humpback fired.
    if (anim === "sleep") {
      lastPick = `wake-stretch · roll ${roll.toFixed(2)}`;
      triggerHumpback();
      onCatWakeAction();
      return;
    }
    if (roll < w.sleep + w.idle) {
      lastPick = `idle · roll ${roll.toFixed(2)}`;
      transitionTo(pickIdle());
      onCatWakeAction();
    } else {
      lastPick = `move · roll ${roll.toFixed(2)}`;
      transitionTo(pickMovement());
      onCatWakeAction();
    }
  }
}

/** Called whenever pickNextBehavior chooses a non-sleep behavior. Sets the
 *  `wokenUp` latch (so future picks have sleep weight = 0) and bumps
 *  `wokeProgress` to shift the idle/move balance further toward move. */
function onCatWakeAction() {
  if (activeLevel < WOKE_PICK_ACTIVE_MAX) {
    if (!wokenUp) {
      wokenUp = true;
      wokeProgress = 0;
    }
    wokeProgress = Math.min(0.85, wokeProgress + WOKE_PROGRESS_STEP);
  }
}

/** Resolves the cat-element `left` value that centers the visible cat over
 *  the clock, clamped so the cat element never extends past the screen. */
function resolveSleepLeft(): number | null {
  if (sleepX === null) return null;
  const elementWidth = catEl.offsetWidth || 96;
  const desired = sleepX - elementWidth / 2;
  const min = 4;
  const max = window.innerWidth - elementWidth - 4;
  return Math.max(min, Math.min(desired, max));
}

/** Walks the cat to the resolved sleep position (above the taskbar clock)
 *  and triggers `sleep` on arrival. If we don't know the clock position yet,
 *  or the cat is already nearby, we sleep on the spot. */
function goToSleep() {
  const sleepLeft = resolveSleepLeft();
  if (sleepLeft !== null && Math.abs(sleepLeft - currentX) > 40) {
    targetX = sleepLeft;
    setFacing(targetX > currentX ? "right" : "left");
    setAnim("run");
    oneShotEndsAt = null;
    stateUntil = Number.POSITIVE_INFINITY;
    pendingSleep = true;
  } else {
    if (sleepLeft !== null) {
      currentX = sleepLeft;
      catEl.style.left = `${currentX}px`;
    }
    transitionTo("sleep");
  }
}

function transitionTo(next: Anim) {
  setAnim(next);
  const spec = ANIM[next];
  if (spec.loops) {
    const dwell = spec.minDwellMs + Math.random() * (spec.maxDwellMs - spec.minDwellMs);
    stateUntil = performance.now() + dwell;
    oneShotEndsAt = null;
    if (spec.speedPx > 0) {
      const screenWidth = window.innerWidth;
      const minX = 20;
      const maxX = Math.max(minX + 120, screenWidth - 140);
      targetX = minX + Math.random() * (maxX - minX);
      setFacing(targetX > currentX ? "right" : "left");
    }
  } else {
    oneShotEndsAt = performance.now() + spec.durationMs;
    stateUntil = oneShotEndsAt + 100;
  }
}

function triggerHumpback() {
  lastHumpback = performance.now();
  transitionTo("humpback");
}

function tick(now: number) {
  if (oneShotEndsAt !== null && now >= oneShotEndsAt) {
    oneShotEndsAt = null;
    pickNextBehavior();
  }

  const speed = ANIM[anim].speedPx;
  if (speed > 0 && !locked) {
    const dir = facing === "right" ? 1 : -1;
    currentX += dir * speed;
    catEl.style.left = `${currentX}px`;
    if ((dir > 0 && currentX >= targetX) || (dir < 0 && currentX <= targetX)) {
      if (franticMode) {
        pickFranticTarget();
      } else if (pendingSleep) {
        pendingSleep = false;
        const sleepLeft = resolveSleepLeft();
        if (sleepLeft !== null) {
          currentX = sleepLeft;
          catEl.style.left = `${currentX}px`;
        }
        transitionTo("sleep");
      } else {
        transitionTo(pickIdle());
      }
    }
  }

  if (oneShotEndsAt === null && now > stateUntil) {
    pickNextBehavior();
  }

  reportSpriteRect();
  if (!toastEl.hidden) {
    // Keep the toast pinned to the cat while it sprints around in frantic
    // mode (or just runs normally).
    positionToast();
  }
  if (!indicatorEl.hidden) {
    positionIndicator();
  }
  updateHeatplate();
  updateDebug();
  requestAnimationFrame(tick);
}

function positionHeatplate() {
  const catRect = catEl.getBoundingClientRect();
  const w = heatplateEl.offsetWidth || 56;
  const h = heatplateEl.offsetHeight || 32;
  // Center horizontally under the cat's visible silhouette. Vertical
  // anchor: glow disc sits roughly at the cat's feet, shimmer extends up.
  const left = catRect.left + catRect.width / 2 - w / 2;
  // The plate's bottom aligns with the cat's bottom (sprite frame bottom).
  // Move up a hair so the disc nestles right under her paws.
  const top = catRect.bottom - h * 0.5;
  heatplateEl.style.left = `${Math.round(left)}px`;
  heatplateEl.style.top = `${Math.round(top)}px`;
}

// ── Hit-test reporting ─────────────────────────────────────────────────────

let lastReported: SpriteRect | null = null;
function reportSpriteRect() {
  // Hit-box covers the bottom-center 20×20 native px of the 32×32 frame —
  // a bit larger than the 12×12 resting silhouette so it's forgiving to
  // click and also covers the upward extent of jump/humpback frames,
  // but doesn't extend into the empty top headroom (which would feel like
  // an invisible too-big button above the cat).
  const HIT_NATIVE = 16;
  const FRAME_NATIVE = 32;
  const catRect = catEl.getBoundingClientRect();
  const padNative = (FRAME_NATIVE - HIT_NATIVE) / 2;
  const padX = catRect.width * (padNative / FRAME_NATIVE);
  const hitH = catRect.height * (HIT_NATIVE / FRAME_NATIVE);
  let left = catRect.left + padX;
  let right = catRect.right - padX;
  let top = catRect.bottom - hitH;
  let bottom = catRect.bottom;

  // Every interactive overlay (chat bubble, reminder toast, quick menu) has
  // to be folded into the Rust click-through hit-test, otherwise clicks
  // on it pass through to the desktop.
  const includeRect = (el: HTMLElement | null) => {
    if (!el || el.hidden) return;
    const r = el.getBoundingClientRect();
    if (r.width === 0 && r.height === 0) return;
    left = Math.min(left, r.left);
    top = Math.min(top, r.top);
    right = Math.max(right, r.right);
    bottom = Math.max(bottom, r.bottom);
  };
  includeRect(bubbleEl);
  includeRect(toastEl);
  includeRect(quickmenuEl);
  includeRect(indicatorEl);

  const next: SpriteRect = {
    x: Math.round(left),
    y: Math.round(top),
    width: Math.round(right - left),
    height: Math.round(bottom - top),
  };
  if (
    !lastReported ||
    next.x !== lastReported.x ||
    next.y !== lastReported.y ||
    next.width !== lastReported.width ||
    next.height !== lastReported.height
  ) {
    lastReported = next;
    ipc.setSpriteRect(next).catch(() => {});
  }
}

// ── Sprite-sheet load detection ────────────────────────────────────────────

function tryLoadSpriteSheet() {
  const img = new Image();
  img.onload = () => {
    if (img.naturalWidth >= 32 && img.naturalHeight >= 32) {
      catEl.setAttribute("data-mode", "sprite");
    }
  };
  img.onerror = () => {};
  img.src = SPRITE_PATH;
}

// ── Thought bubble ─────────────────────────────────────────────────────────

const bubbleEl = document.getElementById("bubble") as HTMLElement;
const messagesEl = document.getElementById("messages") as HTMLElement;
const inputEl = document.getElementById("input") as HTMLTextAreaElement;
const indicatorEl = document.getElementById("status-indicator") as HTMLElement;
const heatplateEl = document.getElementById("heatplate") as HTMLElement;

let streaming = false;
let streamingEl: HTMLDivElement | null = null;
let streamingBuffer = "";
let keyHintShown = false;

// "loading" → request is in flight while the bubble is hidden. Cat stays
// locked (idle, no roaming). "ready" → request finished while bubble was
// hidden, response is buffered in messagesEl waiting for the user to
// re-open. "none" → no pending state.
type PendingNotice = "none" | "loading" | "ready";
let pendingNotice: PendingNotice = "none";

function showIndicator(state: "loading" | "listening" | "ready") {
  indicatorEl.dataset.state = state;
  indicatorEl.hidden = false;
  positionIndicator();
}

function hideIndicator() {
  indicatorEl.hidden = true;
}

function positionIndicator() {
  const catRect = catEl.getBoundingClientRect();
  const contentH =
    parseFloat(
      getComputedStyle(document.documentElement).getPropertyValue("--sprite-content-h"),
    ) || 36;
  const visibleCatTop = catRect.bottom - contentH;
  const w = indicatorEl.offsetWidth || 56;
  const h = indicatorEl.offsetHeight || 56;
  // Centered over the cat's visible silhouette, small gap above the head.
  let left = catRect.left + catRect.width / 2 - w / 2;
  const margin = 4;
  left = Math.max(margin, Math.min(left, window.innerWidth - w - margin));
  const top = Math.max(margin, visibleCatTop - h - 12);
  indicatorEl.style.left = `${left}px`;
  indicatorEl.style.top = `${top}px`;
}
// Each "turn" is a single Q→A. When `conversationComplete` is true at the
// start of `send()`, we wipe both the UI and the backend conversation
// history so the next prompt arrives in a fresh context window.
let conversationComplete = true;

// Headless voice turn — when the user triggers a request via wake-word,
// we buffer the transcript + Claude's streamed response without showing
// the bubble. The indicator shows loading dots while streaming, then a
// "!" once Claude finishes. On bubble-open the buffered conversation is
// rendered. Null when no voice turn is in flight.
type VoiceTurn = { userText: string; assistantBuffer: string };
let voiceTurn: VoiceTurn | null = null;

function isBubbleOpen() {
  return !bubbleEl.hidden;
}

function positionBubble() {
  // The cat *element* is 32×scale px (one full sprite frame), but the cat
  // artwork at rest only fills the bottom-center 12×scale of that — the rest
  // is transparent headroom for action frames. Anchor the bubble to the
  // visible cat's top, not the element's top.
  const catRect = catEl.getBoundingClientRect();
  const bubbleWidth = bubbleEl.offsetWidth || 240;
  const contentH =
    parseFloat(
      getComputedStyle(document.documentElement).getPropertyValue("--sprite-content-h"),
    ) || 36;
  const visibleCatTop = catRect.bottom - contentH;

  // Horizontal anchor: bubble's bottom-left near the cat's right ear so the
  // trail circles descend onto the head. 22/32 = right edge of the 12-wide
  // content within the 32-wide frame.
  let bubbleLeft = catRect.left + catRect.width * (22 / 32);
  const verticalGap = 14;
  const margin = 12;
  bubbleLeft = Math.min(bubbleLeft, window.innerWidth - bubbleWidth - margin);
  bubbleLeft = Math.max(margin, bubbleLeft);
  bubbleEl.style.left = `${bubbleLeft}px`;
  bubbleEl.style.bottom = `${window.innerHeight - visibleCatTop + verticalGap}px`;
}

async function openBubble() {
  if (isBubbleOpen()) return;
  locked = true;
  setFacing("right");
  // Waking the cat by opening the bubble → do a stretch first. After the
  // humpback oneShot ends, the locked picker forces idle1 (see above), so
  // we never need an explicit fallback transition here.
  if (anim === "sleep") {
    triggerHumpback();
  } else {
    transitionTo("idle1");
  }
  bubbleEl.hidden = false;
  hideIndicator();
  positionBubble();

  if (voiceTurn) {
    // Voice-triggered turn — render the buffered conversation. If Claude
    // is still streaming, attach `streamingEl` so subsequent tokens land
    // in the DOM normally; otherwise the turn is complete.
    messagesEl.innerHTML = "";
    renderMessage("user", voiceTurn.userText);
    const assistantEl = renderMessage("assistant", voiceTurn.assistantBuffer);
    if (streaming) {
      assistantEl.classList.add("streaming");
      streamingEl = assistantEl;
      streamingBuffer = voiceTurn.assistantBuffer;
    } else {
      conversationComplete = true;
    }
    voiceTurn = null;
  }
  // Note: we deliberately do NOT wipe messagesEl just because the previous
  // turn finished. send() handles that lazily right before posting the
  // next prompt. This way the user can close+reopen the bubble to re-read
  // an answer without losing it.
  await maybeShowKeyHint();
  inputEl.focus();
}

function closeBubble() {
  if (!isBubbleOpen()) return;
  bubbleEl.hidden = true;
  if (streaming) {
    // Closing mid-request: keep the cat locked (no roaming) and show the
    // pixel "loading" indicator above its head. We'll flip to "ready" when
    // the stream finishes (see finishStreaming below).
    pendingNotice = "loading";
    showIndicator("loading");
    return; // do NOT release the lock
  }
  // Stream is done (or never started). Whether pendingNotice was "ready"
  // (user came back to read) or "none" (normal close), reset both and
  // finalize the turn.
  pendingNotice = "none";
  hideIndicator();
  conversationComplete = true;
  // Release the lock only if no other overlay still holds it.
  if (quickmenuEl.hidden) {
    locked = false;
    pickNextBehavior();
  }
}

function renderMessage(
  role: "user" | "assistant" | "system" | "error" | "hint",
  text: string,
  asHtml = false,
) {
  const div = document.createElement("div");
  div.className = `msg ${role}`;
  if (asHtml) {
    div.innerHTML = text;
  } else if (role === "assistant") {
    renderAssistantBody(div, text);
  } else {
    div.textContent = text;
  }
  messagesEl.appendChild(div);
  scrollToBottom();
  return div;
}

/// Fill an assistant message bubble with markdown text OR extracted cards.
/// **If at least one card parsed**, the prose is dropped entirely — Claude
/// has a habit of duplicating the card's data in prose ("hier die
/// übersicht: …") and the user doesn't want to read it twice. Prose-only
/// responses (no card blocks) render as plain markdown like before.
/// Safe to call repeatedly on the same host (wipes contents first).
function renderAssistantBody(host: HTMLElement, raw: string) {
  host.innerHTML = "";
  const { text, cards } = extractCards(raw);
  if (cards.length > 0) {
    for (const c of cards) {
      host.appendChild(renderCard(c));
    }
    return; // text deliberately suppressed
  }
  if (text) {
    const prose = document.createElement("div");
    prose.className = "prose";
    prose.innerHTML = renderInlineMarkdown(text);
    host.appendChild(prose);
  }
}

// ── Rich cards ───────────────────────────────────────────────────────────
//
// Claude can emit fenced code blocks like ```card-events {…}``` instead of
// (or alongside) plain text. The parser pulls them out, the renderer turns
// each into a compact dark card matching Fibi's pixel-art aesthetic. Three
// types for now: events, reminders, weather.

type CardKind = "events" | "reminders" | "weather";
type Card = { kind: CardKind; data: any };

/** Pull all card blocks out of a message; return the remaining prose. */
function extractCards(raw: string): { text: string; cards: Card[] } {
  const cards: Card[] = [];
  const re = /```card-(events|reminders|weather)\s*\n([\s\S]*?)\n```/g;
  let m: RegExpExecArray | null;
  let cleanText = "";
  let last = 0;
  while ((m = re.exec(raw)) !== null) {
    cleanText += raw.slice(last, m.index);
    try {
      cards.push({ kind: m[1] as CardKind, data: JSON.parse(m[2]) });
    } catch (e) {
      // Malformed JSON → leave the raw block in the prose so the user
      // sees there was an issue rather than silently dropping it.
      cleanText += m[0];
      console.warn("card JSON parse failed:", e);
    }
    last = m.index + m[0].length;
  }
  cleanText += raw.slice(last);
  return { text: cleanText.trim(), cards };
}

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function renderCardEvents(data: any): HTMLElement {
  const card = document.createElement("div");
  card.className = "card card-events";
  const label = (data.label as string | undefined) ?? "Termine";
  const events = Array.isArray(data.events) ? data.events : [];
  card.innerHTML = `
    <div class="card-head">📅 ${escapeHtml(label)}</div>
    <ul class="card-list">
      ${events.length === 0
        ? `<li class="empty">nüscht geplant :3</li>`
        : events.map((e: any) => `
            <li>
              <span class="evt-time">${escapeHtml(String(e.time ?? "–"))}</span>
              <span class="evt-title">${escapeHtml(String(e.title ?? ""))}</span>
              ${e.location ? `<span class="evt-loc">${escapeHtml(String(e.location))}</span>` : ""}
            </li>`).join("")
      }
    </ul>`;
  return card;
}

function renderCardReminders(data: any): HTMLElement {
  const card = document.createElement("div");
  card.className = "card card-reminders";
  const label = (data.label as string | undefined) ?? "Offen";
  const items = Array.isArray(data.items) ? data.items : [];
  card.innerHTML = `
    <div class="card-head">🔔 ${escapeHtml(label)}</div>
    <ul class="card-list">
      ${items.length === 0
        ? `<li class="empty">alles erledigt :3</li>`
        : items.map((it: any) => `
            <li>
              <span class="rem-title">${escapeHtml(String(it.title ?? ""))}</span>
              ${it.due ? `<span class="rem-due">${escapeHtml(String(it.due))}</span>` : ""}
            </li>`).join("")
      }
    </ul>`;
  return card;
}

function renderCardWeather(data: any): HTMLElement {
  const card = document.createElement("div");
  card.className = "card card-weather";
  const icon = String(data.icon ?? "");
  const iconMap: Record<string, string> = {
    sun: "☀️", clear: "☀️",
    cloud: "☁️", cloudy: "☁️", bewölkt: "☁️",
    partly: "⛅", "partly-cloudy": "⛅",
    rain: "🌧️", regen: "🌧️", shower: "🌦️",
    snow: "🌨️", schnee: "🌨️",
    storm: "⛈️", gewitter: "⛈️",
    fog: "🌫️", nebel: "🌫️",
  };
  const iconEmoji = iconMap[icon.toLowerCase()] ?? "🌤️";
  const loc = data.location ? String(data.location) : "";
  const summary = data.summary ? String(data.summary) : "";
  const now = data.now != null ? `${data.now}°` : "";
  const high = data.high != null ? `${data.high}°` : "";
  const low = data.low != null ? `${data.low}°` : "";
  const hourly = Array.isArray(data.hourly) ? data.hourly : [];

  card.innerHTML = `
    <div class="card-head">${iconEmoji} ${escapeHtml(loc)}</div>
    <div class="wx-body">
      <div class="wx-now">${escapeHtml(now)}</div>
      <div class="wx-meta">
        <div class="wx-summary">${escapeHtml(summary)}</div>
        <div class="wx-range">${escapeHtml(high)}${high && low ? " / " : ""}${escapeHtml(low)}</div>
      </div>
    </div>
    ${hourly.length > 0 ? `
      <div class="wx-strip">
        ${hourly.slice(0, 6).map((h: any) => `
          <div class="wx-hour">
            <span class="wx-h">${escapeHtml(String(h.h ?? ""))}</span>
            <span class="wx-icon">${iconMap[String(h.icon ?? "").toLowerCase()] ?? "·"}</span>
            <span class="wx-t">${h.t != null ? escapeHtml(String(h.t)) + "°" : ""}</span>
          </div>`).join("")}
      </div>` : ""}`;
  return card;
}

function renderCard(card: Card): HTMLElement {
  switch (card.kind) {
    case "events": return renderCardEvents(card.data);
    case "reminders": return renderCardReminders(card.data);
    case "weather": return renderCardWeather(card.data);
  }
}

/**
 * Tiny safe inline-markdown renderer. Handles **bold**, *italic*, _italic_,
 * `code` only. Escapes all other HTML. Called on every streaming-buffer
 * update so partial tokens render incrementally — unmatched delimiters at
 * the tail (e.g. mid-stream `**bo`) stay as plain text until the closing
 * `**` arrives in a later token.
 */
function renderInlineMarkdown(text: string): string {
  // 1. Escape HTML.
  let s = text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
  // 2. Inline code first (so backtick contents aren't further processed).
  s = s.replace(/`([^`\n]+?)`/g, "<code>$1</code>");
  // 3. **bold** — non-greedy, doesn't cross newlines.
  s = s.replace(/\*\*([^*\n]+?)\*\*/g, "<strong>$1</strong>");
  // 4. *italic* — single asterisk, not adjacent to another asterisk.
  s = s.replace(/(^|[^*\w])\*(?!\s)([^*\n]+?)(?<!\s)\*(?!\*)/g, "$1<em>$2</em>");
  // 5. _italic_ — single underscore, not inside a word.
  s = s.replace(/(^|[^\w])_(?!\s)([^_\n]+?)(?<!\s)_(?!\w)/g, "$1<em>$2</em>");
  return s;
}

function scrollToBottom() {
  messagesEl.scrollTop = messagesEl.scrollHeight;
}

async function maybeShowKeyHint() {
  if (keyHintShown) return;
  try {
    const hasKey = await ipc.getApiKeyStatus();
    if (!hasKey) {
      renderMessage(
        "hint",
        'Kein API-Key gefunden. Lege im Projekt-Root eine <code>.env</code> mit <code>ANTHROPIC_API_KEY=sk-ant-...</code> ab und starte neu.',
        true,
      );
      keyHintShown = true;
    }
  } catch (err) {
    console.error("getApiKeyStatus failed", err);
  }
}

async function send() {
  if (streaming) return;
  const text = inputEl.value.trim();
  if (!text) return;

  // New turn → wipe both the view and the backend history so Claude
  // doesn't carry context from the previous Q→A into this one.
  if (conversationComplete) {
    messagesEl.innerHTML = "";
    try {
      await ipc.clearHistory();
    } catch {
      /* if the clear fails, we'll just have leftover history this turn */
    }
  }
  conversationComplete = false;

  inputEl.value = "";
  inputEl.style.height = "auto";
  renderMessage("user", text);

  streamingBuffer = "";
  streamingEl = renderMessage("assistant", "");
  streamingEl.classList.add("streaming");
  streaming = true;

  try {
    await ipc.sendMessage(text);
  } catch (err) {
    const msg = typeof err === "string" ? err : String(err);
    finishStreaming(null);
    if (msg.includes("api-key-missing")) {
      renderMessage(
        "hint",
        'Kein API-Key. Lege <code>.env</code> mit <code>ANTHROPIC_API_KEY=...</code> an und starte neu.',
        true,
      );
    } else {
      renderMessage("error", msg);
    }
  }
}

function finishStreaming(finalText: string | null) {
  if (streamingEl) {
    streamingEl.classList.remove("streaming");
    if (finalText !== null) {
      // Final render: extract card blocks and render them as rich UI.
      renderAssistantBody(streamingEl, finalText);
    }
    streamingEl = null;
  }
  streaming = false;
  streamingBuffer = "";
  if (bubbleEl.hidden && pendingNotice === "loading") {
    // Bubble was closed mid-request. Flip the indicator to "ready" so the
    // user sees an exclamation mark above the cat's head. Do NOT mark the
    // turn complete yet — that only happens once they actually read it
    // (i.e. open the bubble and then close it again).
    pendingNotice = "ready";
    showIndicator("ready");
  } else {
    // Either bubble is still open (normal path), or there was no pending
    // notice. Either way, the turn is done.
    conversationComplete = true;
  }
}

// ── Voice ────────────────────────────────────────────────────────────────
//
// Wake word fires from the Python sidecar. Pipeline (Rust):
//   wake → record→VAD → whisper → emit voice://transcript with text
//   wake → no speech / error      → emit voice://cancel
//
// Behavior (HEADLESS — bubble stays closed):
//   1. Wake → show pixel "loading dots" above Fibi.
//   2. Whisper transcribes, then we call ipc.sendMessage(text) silently.
//   3. Claude tokens stream into `voiceTurn.assistantBuffer`, NOT into the
//      DOM (bubble is hidden).
//   4. "done" → flip indicator to "!" so David sees there's an answer ready.
//   5. David clicks Fibi → openBubble renders the buffered Q+A.

void ipc.onVoiceWake(() => {
  if (streaming) return; // already chatting, ignore
  // Dismiss any previous voice answer toast — new question, new answer.
  // Reminder toasts are left alone (they want acknowledgement).
  if (!toastEl.hidden && franticTimer === null) {
    hideToast();
  }
  // Wake-from-sleep stretch — same animation as a click on the sleeping
  // cat. Fires regardless of bubble state so Fibi reacts to her name even
  // when she's snoozing over the clock.
  if (anim === "sleep") {
    triggerHumpback();
  } else if (!locked) {
    // Snap to an awake idle frame so she's visibly attentive.
    transitionTo("idle1");
  }
  // Lock her for the duration of the voice turn — picker stays paused
  // through listening and thinking; releaseVoiceLock fires on done /
  // error / cancel below.
  locked = true;
  onCatWakeAction();
  if (isBubbleOpen()) {
    // Bubble open: voice will go through the regular typed-prompt flow
    // once the transcript arrives. No indicator needed.
    return;
  }
  // Mic is hot, Whisper is transcribing — show the "listening" bars.
  pendingNotice = "loading";
  showIndicator("listening");
});

/// Release the picker lock if no other overlay (bubble, quick menu) is
/// still holding it. Mirrors closeBubble's release path so we don't
/// fight over the flag.
function releaseVoiceLock() {
  if (!isBubbleOpen() && quickmenuEl.hidden) {
    locked = false;
    pickNextBehavior();
  }
}

void ipc.onVoiceTranscript((text) => {
  const trimmed = text.trim();
  if (!trimmed) {
    pendingNotice = "none";
    hideIndicator();
    return;
  }
  if (isBubbleOpen()) {
    // Bubble visible — treat as normal typed prompt.
    inputEl.value = trimmed;
    void send();
    return;
  }
  // Headless: send without opening the bubble, accumulate Claude's reply
  // in `voiceTurn`, indicator stays "loading" until "done" arrives.
  submitVoiceTextHeadless(trimmed);
});

void ipc.onVoiceCancel((reason) => {
  if (voiceTurn) return; // already past the recording stage
  pendingNotice = "none";
  hideIndicator();
  console.debug("voice cancelled:", reason);
  releaseVoiceLock();
});

async function submitVoiceTextHeadless(text: string) {
  voiceTurn = { userText: text, assistantBuffer: "" };
  // Transcript is in, the mic is done — switch from "listening" bars
  // to "thinking" dots while Claude generates the answer.
  pendingNotice = "loading";
  showIndicator("loading");
  streaming = true;
  // Wipe backend history so this voice turn starts fresh (matches the
  // behavior of typed send() when conversationComplete is true).
  if (conversationComplete) {
    try {
      await ipc.clearHistory();
    } catch {
      /* non-fatal */
    }
  }
  conversationComplete = false;
  try {
    await ipc.sendMessage(text);
  } catch (err) {
    console.error("voice sendMessage failed:", err);
    voiceTurn = null;
    streaming = false;
    pendingNotice = "none";
    hideIndicator();
    releaseVoiceLock();
  }
}

ipc.onChatToken((token: ChatToken) => {
  // Headless voice path: bubble is closed, accumulate into voiceTurn buffer.
  if (voiceTurn && !isBubbleOpen()) {
    if (token.type === "delta") {
      voiceTurn.assistantBuffer += token.text;
    } else if (token.type === "done") {
      // Voice answer surfaces as a toast next to Fibi — no chat-bubble
      // open, no input field, just the response. User clicks to dismiss
      // (or asks the next voice question, which replaces it).
      // Importantly: we do NOT touch messagesEl here. Voice and chat
      // are independent surfaces; the user's typed-chat history must
      // survive voice turns untouched.
      const answer = voiceTurn.assistantBuffer.trim();
      voiceTurn = null;
      streaming = false;
      pendingNotice = "none";
      hideIndicator();
      conversationComplete = true;
      if (answer) {
        showToast(answer, { franticOnIgnore: false, markdown: true });
      }
      releaseVoiceLock();
    } else if (token.type === "error") {
      console.error("voice chat error:", token.message);
      voiceTurn = null;
      streaming = false;
      pendingNotice = "none";
      hideIndicator();
      releaseVoiceLock();
    }
    return;
  }
  // Normal DOM-streaming path (bubble open).
  if (!streamingEl && token.type !== "error") return;
  if (token.type === "delta") {
    streamingBuffer += token.text;
    if (streamingEl) {
      streamingEl.innerHTML = renderInlineMarkdown(streamingBuffer);
      scrollToBottom();
    }
  } else if (token.type === "done") {
    finishStreaming(streamingBuffer);
  } else if (token.type === "error") {
    finishStreaming(null);
    renderMessage("error", token.message);
  }
});

// ── Interactions ───────────────────────────────────────────────────────────

catEl.addEventListener("click", (event) => {
  event.preventDefault();
  if (isBubbleOpen()) {
    closeBubble();
    return;
  }
  void openBubble();
});

// Clicking the indicator (loading dots or "!") opens the bubble — same as
// clicking the cat itself. Stops propagation so the click doesn't also
// bubble up to the window blur handler.
indicatorEl.addEventListener("click", (event) => {
  event.preventDefault();
  event.stopPropagation();
  void openBubble();
});

catEl.addEventListener("dblclick", (event) => {
  event.preventDefault();
  if (isBubbleOpen()) return;
  goToSleep();
});

catEl.addEventListener("mouseenter", () => {
  if (isBubbleOpen()) return;
  // Don't startle a sleeping cat with a humpback — let her sleep.
  if (anim === "sleep") return;
  const now = performance.now();
  if (now - lastHumpback < HUMPBACK_COOLDOWN_MS) return;
  if (oneShotEndsAt !== null) return;
  triggerHumpback();
});

// Clicking the bubble's empty area (anywhere that isn't a message bubble
// or the input itself) closes it. The check `e.target === bubbleEl` makes
// sure we only fire for hits on the container, not on its children.
bubbleEl.addEventListener("click", (event) => {
  if (event.target === bubbleEl) closeBubble();
});

// Clicking outside the cat window entirely (other app, desktop) gives that
// window focus and blurs ours — close the bubble in that case too.
window.addEventListener("blur", () => {
  if (isBubbleOpen()) closeBubble();
  if (!quickmenuEl.hidden) hideQuickmenu();
});

inputEl.addEventListener("input", () => {
  inputEl.style.height = "auto";
  inputEl.style.height = `${Math.min(inputEl.scrollHeight, 80)}px`;
});

inputEl.addEventListener("keydown", (event) => {
  if (event.key === "Enter" && !event.shiftKey) {
    event.preventDefault();
    void send();
  }
});

document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && isBubbleOpen()) {
    closeBubble();
  }
});

// ── Quick menu (right-click on cat) ──────────────────────────────────────

const quickmenuEl = document.getElementById("quickmenu") as HTMLElement;

function showQuickmenu(anchorX: number, anchorY: number) {
  quickmenuEl.hidden = false;
  const width = quickmenuEl.offsetWidth || 170;
  const height = quickmenuEl.offsetHeight || 160;
  const margin = 8;
  let left = anchorX - width / 2;
  let top = anchorY - height - 8;
  left = Math.max(margin, Math.min(left, window.innerWidth - width - margin));
  top = Math.max(margin, Math.min(top, window.innerHeight - height - margin));
  quickmenuEl.style.left = `${left}px`;
  quickmenuEl.style.top = `${top}px`;
  // Freeze the cat where it is so the menu doesn't drift away while open.
  locked = true;
  transitionTo("idle1");
}

function hideQuickmenu() {
  quickmenuEl.hidden = true;
  // Only release the lock if the chat bubble isn't also keeping it held.
  if (!isBubbleOpen()) {
    locked = false;
    pickNextBehavior();
  }
}

catEl.addEventListener("contextmenu", (event) => {
  event.preventDefault();
  if (!quickmenuEl.hidden) {
    hideQuickmenu();
    return;
  }
  showQuickmenu(event.clientX, event.clientY);
});

quickmenuEl.addEventListener("click", (event) => {
  const target = event.target as HTMLElement;
  const button = target.closest<HTMLButtonElement>(".qm-item");
  if (!button) return;
  hideQuickmenu();
  const prompt = QUICKMENU_PROMPTS[button.dataset.action ?? ""];
  if (prompt) {
    void runQuickPrompt(prompt);
  }
});

/// Canned prompts triggered by the right-click quick menu. Each goes
/// through the same chat flow as a typed prompt — opens the bubble,
/// fills the input, hits send — so the answer renders as a card (or
/// whatever Claude decides) inside the chat bubble.
const QUICKMENU_PROMPTS: Record<string, string> = {
  today: "Was steht heute an?",
  week: "Was steht diese Woche an?",
  weather: "Wie wird das Wetter heute?",
  reminders: "Welche Reminder sind offen?",
  mail: "Hab ich neue Mails?",
};

async function runQuickPrompt(text: string) {
  if (streaming) return;
  if (!isBubbleOpen()) {
    await openBubble();
  }
  inputEl.value = text;
  void send();
}

// Click anywhere else inside our window closes the quick menu.
document.addEventListener("click", (event) => {
  if (quickmenuEl.hidden) return;
  if (event.target instanceof Node && quickmenuEl.contains(event.target)) return;
  hideQuickmenu();
});

// Same for ESC.
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && !quickmenuEl.hidden) {
    hideQuickmenu();
  }
});

// ── Reminder toast ────────────────────────────────────────────────────────

const toastEl = document.getElementById("toast") as HTMLElement;
const toastTextEl = document.getElementById("toast-text") as HTMLElement;
const FRANTIC_AFTER_MS = 30_000;
const TOAST_SMOOTHING = 0.18; // 18 % of the gap closed per frame ≈ 250 ms tail
let franticTimer: number | null = null;
let toastDisplayLeft: number | null = null;
let toastDisplayBottom: number | null = null;

function showToast(text: string, opts: { franticOnIgnore?: boolean; markdown?: boolean } = {}) {
  const { franticOnIgnore = true, markdown = false } = opts;
  if (markdown) {
    // Same path as the chat-bubble assistant render — extracts card-*
    // blocks and turns them into compact widgets next to any prose.
    renderAssistantBody(toastTextEl, text);
  } else {
    toastTextEl.textContent = text;
  }
  toastEl.hidden = false;
  // Reset smoothing state so the toast snaps to the cat on first show
  // rather than gliding in from wherever it was last placed.
  toastDisplayLeft = null;
  toastDisplayBottom = null;
  positionToast();
  if (franticTimer !== null) {
    window.clearTimeout(franticTimer);
    franticTimer = null;
  }
  // Reminders escalate Fibi to frantic mode if ignored. Voice answers do
  // NOT — they're informational and the user can dismiss at their leisure.
  if (franticOnIgnore) {
    franticTimer = window.setTimeout(() => {
      franticTimer = null;
      enterFranticMode();
    }, FRANTIC_AFTER_MS);
  }
}

function hideToast() {
  toastEl.hidden = true;
  if (franticTimer !== null) {
    window.clearTimeout(franticTimer);
    franticTimer = null;
  }
  if (franticMode) {
    exitFranticMode();
  }
}

function enterFranticMode() {
  if (franticMode) return;
  franticMode = true;
  // Force an immediate run regardless of what the cat is doing now.
  pendingSleep = false;
  oneShotEndsAt = null;
  pickFranticTarget();
}

function exitFranticMode() {
  if (!franticMode) return;
  franticMode = false;
  // Force a regular pick so the cat doesn't keep its Infinity dwell.
  pickNextBehavior();
}

/// Picks a target on the *opposite* side of the screen from where the cat
/// currently is, so each leg of the frantic sprint crosses the full width.
function pickFranticTarget() {
  const screenWidth = window.innerWidth;
  const elementWidth = catEl.offsetWidth || 96;
  const minX = 12;
  const maxX = screenWidth - elementWidth - 12;
  // 20 % band on the far side.
  const target = currentX > screenWidth / 2
    ? minX + Math.random() * (screenWidth * 0.2)
    : maxX - Math.random() * (screenWidth * 0.2);
  targetX = Math.max(minX, Math.min(target, maxX));
  setFacing(targetX > currentX ? "right" : "left");
  setAnim("run-long");
  oneShotEndsAt = null;
  // Infinity ensures the standard dwell-check never aborts the sprint —
  // only reaching the target (in tick) advances to the next leg.
  stateUntil = Number.POSITIVE_INFINITY;
}

function positionToast() {
  // Recompute the *target* anchor each frame (cat moves, target moves).
  const catRect = catEl.getBoundingClientRect();
  const toastWidth = toastEl.offsetWidth || 240;
  const contentH =
    parseFloat(
      getComputedStyle(document.documentElement).getPropertyValue("--sprite-content-h"),
    ) || 36;
  const visibleCatTop = catRect.bottom - contentH;

  // Trail behind the direction of motion: cat facing right → toast on the
  // left side of the cat; cat facing left → toast on the right side. The
  // gap is a few px so the toast doesn't sit flush against the cat body.
  const gap = 8;
  const margin = 12;
  let targetLeft = facing === "right"
    ? catRect.left - gap - toastWidth
    : catRect.right + gap;
  targetLeft = Math.min(targetLeft, window.innerWidth - toastWidth - margin);
  targetLeft = Math.max(margin, targetLeft);
  const targetBottom = window.innerHeight - visibleCatTop + 14;

  // Lerp the displayed position toward the target so the toast trails the
  // cat with a bit of friction instead of teleporting every frame.
  if (toastDisplayLeft === null || toastDisplayBottom === null) {
    toastDisplayLeft = targetLeft;
    toastDisplayBottom = targetBottom;
  } else {
    toastDisplayLeft += (targetLeft - toastDisplayLeft) * TOAST_SMOOTHING;
    toastDisplayBottom += (targetBottom - toastDisplayBottom) * TOAST_SMOOTHING;
    // Snap when close enough to avoid sub-pixel jitter forever.
    if (Math.abs(targetLeft - toastDisplayLeft) < 0.4) toastDisplayLeft = targetLeft;
    if (Math.abs(targetBottom - toastDisplayBottom) < 0.4) toastDisplayBottom = targetBottom;
  }
  toastEl.style.left = `${toastDisplayLeft}px`;
  toastEl.style.bottom = `${toastDisplayBottom}px`;
}

toastEl.addEventListener("click", () => hideToast());

// ── CPU heat indicator ───────────────────────────────────────────────────
//
// Backend samples global CPU usage every ~2 s and emits a `cat://heat`
// payload whenever the smoothed state changes (cool/warm/hot). We update
// the hot-plate's data-state and Fibi's own filter class to match.

type HeatState = "cool" | "warm" | "hot";

// Current heat from the Rust sampler. Visibility of the plate is a
// function of (this state !== "cool") AND (anim === "sleep") — Fibi only
// gets warmed from below while she's actually lying on the taskbar.
// Evaluated each frame in tick() so anim transitions are picked up.
let currentHeat: HeatState = "cool";

void listen<{ state: HeatState; cpu: number }>("cat://heat", (event) => {
  currentHeat = event.payload.state;
  // Keep data-state on the element in sync so the gradient/animation
  // hooks (warm vs hot) are ready when it next fades in.
  if (currentHeat !== "cool") {
    heatplateEl.dataset.state = currentHeat;
  }
});

function updateHeatplate() {
  const active = currentHeat !== "cool" && anim === "sleep";
  heatplateEl.classList.toggle("visible", active);
  catEl.classList.toggle("heat-warm", active && currentHeat === "warm");
  catEl.classList.toggle("heat-hot", active && currentHeat === "hot");
  if (active) {
    positionHeatplate();
  }
}

void listen<{ kind: string; text: string }>("cat://reminder", (event) => {
  // If the cat is sleeping, do a wake-up stretch first — feels more alive
  // than a hard cut to idle. Picker takes over after the humpback ends.
  if (anim === "sleep" && !isBubbleOpen()) {
    triggerHumpback();
  }
  showToast(event.payload.text);
});

// Rust fires `cat://will-hide` ~120 ms before it hides the window for a
// fullscreen / presentation. We use that window to pre-position the cat
// off-screen on the right and queue up a run, so when the window is shown
// again (fullscreen ended) the cat runs in from the right edge instead of
// snapping back to its old position.
void listen("cat://will-hide", () => {
  if (locked || isBubbleOpen()) return;
  enterFromRight();
});

function enterFromRight() {
  pendingSleep = false; // entrance overrides any in-flight walk-to-sleep
  const screenWidth = window.innerWidth;
  currentX = screenWidth + 40;
  catEl.style.left = `${currentX}px`;
  setFacing("left");
  setAnim("run");
  // Land somewhere in the left 40 % of the screen so the entrance feels
  // intentional (rather than the cat appearing two pixels in).
  targetX = 60 + Math.random() * (screenWidth * 0.4);
  oneShotEndsAt = null;
  // The "reached target" path in tick() owns the transition to idle.
  // Infinity here means the dwell check can't fire mid-entrance, even if the
  // user stays in fullscreen for hours.
  stateUntil = Number.POSITIVE_INFINITY;
}

// ── Debug overlay ──────────────────────────────────────────────────────────

const debugEl = document.getElementById("debug") as HTMLElement;
const dbg = {
  idle: document.getElementById("dbg-idle") as HTMLElement,
  active: document.getElementById("dbg-active") as HTMLElement,
  woken: document.getElementById("dbg-woken") as HTMLElement,
  anim: document.getElementById("dbg-anim") as HTMLElement,
  weights: document.getElementById("dbg-weights") as HTMLElement,
  lastPick: document.getElementById("dbg-last-pick") as HTMLElement,
  next: document.getElementById("dbg-next") as HTMLElement,
  pos: document.getElementById("dbg-pos") as HTMLElement,
  sleepX: document.getElementById("dbg-sleep-x") as HTMLElement,
  flags: document.getElementById("dbg-flags") as HTMLElement,
  context: document.getElementById("dbg-context") as HTMLElement,
  cals: document.getElementById("dbg-cals") as HTMLElement,
};
let debugVisible = false;

function toggleDebug() {
  debugVisible = !debugVisible;
  debugEl.hidden = !debugVisible;
  if (debugVisible) void refreshDebugContext();
}

// Right-click on the cat opens the quick menu (handler below). The debug
// overlay is reachable via the tray icon instead.
void listen("cat://toggle-debug", () => toggleDebug());

async function refreshDebugContext() {
  try {
    const ctx = await ipc.getRuntimeContext();
    dbg.context.textContent = ctx;
    dbg.context.style.whiteSpace = "pre-wrap";
  } catch (err) {
    dbg.context.textContent = `error: ${String(err)}`;
  }
  dbg.cals.textContent = "lade…";
  dbg.cals.style.whiteSpace = "pre-wrap";
  try {
    dbg.cals.textContent = await ipc.getCalendarSources();
  } catch (err) {
    dbg.cals.textContent = `error: ${String(err)}`;
  }
}

function updateDebug() {
  if (!debugVisible) return;
  const w = behaviorWeights();
  const pct = (n: number) => `${Math.round(n * 100)}%`;
  dbg.idle.textContent = `${userIdleSecs}s`;
  dbg.active.textContent = activeLevel.toFixed(2);
  dbg.woken.textContent = wokenUp ? `yes · progress ${wokeProgress.toFixed(2)}` : "no";
  dbg.anim.textContent = anim;
  dbg.weights.textContent = `sleep ${pct(w.sleep)} · idle ${pct(w.idle)} · move ${pct(w.move)}`;
  dbg.lastPick.textContent = lastPick;
  if (stateUntil === Number.POSITIVE_INFINITY) {
    dbg.next.textContent = "∞ (target reach)";
  } else {
    const dueIn = Math.max(0, stateUntil - performance.now()) / 1000;
    dbg.next.textContent = `${dueIn.toFixed(1)}s`;
  }
  const moving = ANIM[anim].speedPx > 0;
  dbg.pos.textContent = moving
    ? `${Math.round(currentX)} → ${Math.round(targetX)}`
    : `${Math.round(currentX)}`;
  dbg.sleepX.textContent = sleepX !== null ? String(sleepX) : "unknown";
  const flags: string[] = [];
  if (locked) flags.push("locked");
  if (pendingSleep) flags.push("pendingSleep");
  if (oneShotEndsAt !== null) flags.push("oneShot");
  if (isBubbleOpen()) flags.push("bubble");
  dbg.flags.textContent = flags.length ? flags.join(" ") : "—";
}

// ── Polling user state ────────────────────────────────────────────────────

async function pollIdle() {
  try {
    userIdleSecs = await ipc.getIdleSeconds();
  } catch {
    /* keep last value */
  }
  // Smoothly blend activeLevel toward the target, faster when going up
  // (user just came back — cat should head for the clock) than when going
  // down (idle build-up stays gradual).
  const target = targetActiveLevel(userIdleSecs);
  const blend = target > activeLevel ? ACTIVE_BLEND_UP : ACTIVE_BLEND_DOWN;
  const wasBelowReset = activeLevel < RESET_ACTIVE_THRESHOLD;
  activeLevel = activeLevel + (target - activeLevel) * blend;
  const nowAboveReset = activeLevel >= RESET_ACTIVE_THRESHOLD;

  // Reset the wake-session latch once the user is clearly back.
  if (wokenUp && nowAboveReset) {
    wokenUp = false;
    wokeProgress = 0;
  }

  // The moment we cross into "active" territory, force a behavior re-pick
  // instead of waiting out the current dwell — otherwise the cat might keep
  // running for several seconds before noticing the user is back. Skip if
  // the cat is in the middle of a one-shot or already on its way to sleep.
  if (
    wasBelowReset &&
    nowAboveReset &&
    !locked &&
    !pendingSleep &&
    oneShotEndsAt === null
  ) {
    pickNextBehavior();
  }
}

async function refreshSleepPosition() {
  try {
    sleepX = await ipc.getSleepPositionX();
  } catch {
    /* keep last value */
  }
}

// ── Boot ──────────────────────────────────────────────────────────────────

async function boot() {
  catEl.classList.add(`anim-${anim}`);
  catEl.classList.add(`facing-${facing}`);
  tryLoadSpriteSheet();

  // Resolve the clock position and current idle time before the first
  // behavior pick — otherwise the cat would always pick "active user"
  // weights on first boot and sleep at left:40 (the default position).
  await Promise.all([refreshSleepPosition(), pollIdle()]);

  pickNextBehavior();
  // Spawn entrance: Fibi runs in from the right edge instead of popping
  // into existence wherever the picker put her. Overrides the pick above
  // (same as cat://will-hide does after a fullscreen exit).
  enterFromRight();
  requestAnimationFrame(tick);

  // Refresh on a slow cadence — idle every 4 s for responsive ramp-up,
  // clock position every minute since the taskbar rarely moves.
  setInterval(pollIdle, 4_000);
  setInterval(refreshSleepPosition, 60_000);
}

void boot();
