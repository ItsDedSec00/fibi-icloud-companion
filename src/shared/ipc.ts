import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type SpriteRect = {
  x: number;
  y: number;
  width: number;
  height: number;
};

export type StoredMessage = {
  id: number;
  role: "user" | "assistant" | "system";
  content: string;
  created_at: number;
};

export type ChatToken =
  | { type: "delta"; text: string }
  | { type: "done" }
  | { type: "error"; message: string };

export const ipc = {
  setSpriteRect: (rect: SpriteRect) => invoke<void>("set_sprite_rect", { rect }),
  listMessages: () => invoke<StoredMessage[]>("list_messages"),
  clearHistory: () => invoke<void>("clear_history"),
  getApiKeyStatus: () => invoke<boolean>("get_api_key_status"),
  getModel: () => invoke<{ model: string }>("get_model"),
  sendMessage: (text: string) => invoke<void>("send_message", { text }),
  getSleepPositionX: () => invoke<number | null>("get_sleep_position_x"),
  getIdleSeconds: () => invoke<number>("get_idle_seconds"),
  getRuntimeContext: () => invoke<string>("get_runtime_context"),
  getCalendarSources: () => invoke<string>("get_calendar_sources"),
  onChatToken: (handler: (token: ChatToken) => void): Promise<UnlistenFn> =>
    listen<ChatToken>("chat://token", (event) => handler(event.payload)),
  /** Wake-word detected — payload is the detection score. */
  onVoiceWake: (handler: (score: number) => void): Promise<UnlistenFn> =>
    listen<number>("voice://wake", (event) => handler(event.payload)),
  /** Whisper transcribed the post-wake utterance — payload is the text. */
  onVoiceTranscript: (handler: (text: string) => void): Promise<UnlistenFn> =>
    listen<string>("voice://transcript", (event) => handler(event.payload)),
  /** No speech captured (or transcription error) — payload is a reason string. */
  onVoiceCancel: (handler: (reason: string) => void): Promise<UnlistenFn> =>
    listen<string>("voice://cancel", (event) => handler(event.payload)),
};
