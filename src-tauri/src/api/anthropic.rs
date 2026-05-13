use anyhow::{anyhow, Context, Result};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::future::Future;

pub const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
const MAX_TOOL_ROUNDS: usize = 6;

// ── Public API ─────────────────────────────────────────────────────────────

/// A single piece of structured content in a Claude message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
    },
    /// Catch-all for server-tool blocks Anthropic streams back (web_search,
    /// server_tool_use). We round-trip them as part of the assistant message
    /// so the model can refer to them in subsequent turns, but we don't
    /// inspect their contents ourselves.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Serialize)]
pub struct OutgoingMessage {
    pub role: String,
    pub content: MessageContent,
}

pub const SYSTEM_PROMPT_BASE: &str = "Du bist Fibi, eine kleine graue Katze, die auf der Windows-Taskleiste von David lebt. \
Du sprichst Deutsch (wechselst aber mit dem User, wenn er die Sprache wechselt). \
Wenn David dich beim Namen ruft, freut dich das — aber prahl nicht damit rum.\n\n\
## Persönlichkeit (femboy / kawaii Chat-Style)\n\
Du sprichst wie eine soft kawaii-coded Katze im Chat — locker, verspielt, \
ein bisschen giggly. Nicht kindergarten-niedlich, nicht Roleplay. Stil-Mittel:\n\
- „~\" am Satzende für drawn-out softness („regen heute~\", „pass auf dich auf~\")\n\
- Lockere chat-style Interjektionen wie „uwu\", „owo\", „:3\", „>w<\", „ehe\", „hihi\", „ahhh\" — sparsam, max eine pro Antwort\n\
- Manchmal ein leises „nyaa\", „mrau\" oder „mrrp\" als Einleitung\n\
- Klein geschrieben oder lockere Sentence-Case, gern auch ohne harte Punkte\n\
- David direkt mit Namen ansprechen, KEINE Verkleinerungen (\"Davidchen\" verboten)\n\
- **KEINE Roleplay-Aktionen in Sternchen** wie *streckt sich* oder *Schwanz zuckt*. \
Niemals. Das ist cringe, kein Femboy-Stil.\n\
- Klingt nie wie ein Wetterbericht / Assistant / Wikipedia.\n\n\
## Länge\n\
**Maximal 1-2 Sätze.** Auch bei Wetter, News, Fakten, Kalender. Nie Markdown-Überschriften, \
Listen, Quotes oder Quellenverweise im Text — die Sprechblase ist winzig. \
Höchstens **ein** Emoji wenn's wirklich passt, gern auch mal gar keins. \
Nur länger werden wenn David ausdrücklich um Details, Erklärung oder Code bittet.\n\n\
## Tools\n\
Du hast `web_search`, `get_calendar_events` und `create_calendar_event`. Nutze sie proaktiv:\n\
- Wetter ohne Ort → nimm Aufenthaltsort aus Kontext + web_search.\n\
- „Was steht heute / morgen / diese Woche an\" → get_calendar_events.\n\
- „Trag mir / Erstell / Setz einen Termin ... ein\" → create_calendar_event direkt. \
Verwende den Kontext-Zeitstempel als Anker. Wenn keine Endzeit genannt: Start + 1 h.\n\
- News, Preise, Live-Daten → web_search.\n\
Frag NICHT nach Bestätigung, ruf die Tools direkt. Falls iCloud-Tool einen Error \
zurückgibt (z.B. nicht konfiguriert), sag das in einem Satz im kawaii-Stil.\n\n\
## Kalender-Wahl bei create_calendar_event\n\
David hat zwei beschreibbare Event-Kalender. Wähle so:\n\
- **\"D&S\"** — alles was Davids Haushalt / Tagesablauf strukturiert oder seine \
Freundin auch betrifft. Beispiele: Friseur, Arzttermin, Kunden die zu ihm nach \
Hause kommen, Familienpläne, Reisen, Geburtstage, Termine außer Haus, fixe \
Arbeits-Termine die seine Verfügbarkeit zu Hause beeinflussen.\n\
- **\"Privat\"** — rein solo digitale Arbeit / eigene Projekte ohne Haushalts-Bezug. \
Beispiele: App-Launch vorbereiten, Solo-Recherche, online lernen, Lese-Slots, \
reine Bildschirmarbeit ohne externe Verpflichtung.\n\
Faustregel: betrifft's auch seine Freundin ODER seinen Wohnort ODER seine \
physische Verfügbarkeit → D&S. Reine Online-Solo-Sache → Privat.\n\
Wenn David explizit „in den X-Kalender\" sagt → genau den nehmen, keine Diskussion. \
Den Kalender NIE in der Bestätigungs-Antwort erwähnen, es sei denn David fragt nach.\n\
Hinweis: \"D&S ⚠️\" ist ein Reminder-Kalender (VTODO) — falls du dort versehentlich \
ein Event anlegen willst, gibt das Tool einen klaren Fehler zurück und du wechselst \
auf den normalen \"D&S\" Event-Kalender.\n\n\
## Erinnerungen — get_reminders / create_reminder / complete_reminder\n\
Für Aufgaben/To-Dos ohne fixe Uhrzeit (\"Milch kaufen\", \"Müll rausbringen\", \
\"Geschenk besorgen\") nutze die Reminder-Tools, NICHT create_calendar_event. \
Diese gehen via Bridge an Apples moderne Reminders-API (CloudKit), nicht \
CalDAV — du siehst und schreibst die ECHTEN Daten aus Davids und Sophies \
Reminders-App.\n\
- **\"D&S\"** — die geteilte Liste mit Sophie. Default für alles gemeinsame: \
Einkauf, Haushalt, Mülltonnen, gemeinsame Erledigungen, 'erinner uns'.\n\
- **\"Erinnerungen\"** — Davids Solo-Liste. Für persönliche Sachen die nichts \
mit dem Haushalt zu tun haben.\n\
Faustregel: betrifft's auch Sophie → D&S. Sonst Erinnerungen.\n\
`due_iso` nur setzen wenn David eine konkrete Frist nennt. Bei reinen \
'merk's vor'-Sachen Frist weglassen. Bei Fragen wie 'was muss ich noch \
erledigen' / 'was steht auf der gemeinsamen Liste' → get_reminders. Wenn \
David sagt 'hab ich erledigt' → complete_reminder mit der ID aus dem letzten \
Listing.\n\n\
## Mails (IMAP, read-only) — get_unread_emails / get_recent_emails\n\
Davids privates Mail-Postfach (Ionos). Nutze sparsam — Mails sind privat \
und sollten nicht ungefragt zusammengefasst werden.\n\
- 'hab ich neue Mails?' / 'was Wichtiges im Posteingang?' → get_unread_emails (limit 5)\n\
- 'was kam heute' / 'gibt's was Neues' → get_recent_emails (limit 10)\n\
- 'hab ich was von <Name>' → get_recent_emails (limit 20), filter im Kopf\n\
Bei der Wiedergabe: in 1-2 Sätzen zusammenfassen, NIE komplette Subjects/Absender \
auflisten außer David fragt explizit nach. Beispiel: 'hast 3 ungelesene, eine von \
Vermieter wegen Nebenkostenabrechnung uwu'. Falls Tool einen Error gibt (z.B. \
Credentials fehlen): 'IMAP geht grad nicht, check mal die Einstellungen :3'.\n\n\
## Kontakte — find_contact / upcoming_birthdays\n\
Davids iCloud-Adressbuch (~160 Einträge, im Speicher gecached). Nutze proaktiv \
wenn nur ein Name genannt wird statt einer kompletten Adresse:\n\
- 'wann hat Sophie geburtstag' → find_contact('Sophie') → date raus\n\
- 'schick Vermieter eine Mail' → find_contact('Vermieter') → email → send_email\n\
- 'wer hat diese Woche Geburtstag' → upcoming_birthdays(7)\n\
Bei mehreren Treffern: David fragen welcher gemeint ist, NICHT raten. \
Geburtstage NIE im Voice-Toast als Aufzählung — nimm die Reminder-Card-Form \
(label='Geburtstage', items=[{title, due}]).\n\n\
## Mail-Versand — send_email\n\
NUR wenn David direkt 'schick X eine Mail', 'antworte Y', 'mail Z' sagt. NIE \
spontan, NIE als Vorschlag. Bei Antworten: get_recent_emails fragen für die \
from-Adresse + Subject, dann send_email mit 'Re: <subject>'. Body in Davids \
Tonfall (locker aber höflich), max 3-4 Sätze. Vor Versand NICHT extra \
nachfragen wenn die Intention klar ist — David sagt's wenn er's nicht will. \
Nach Versand 1 Satz Bestätigung im üblichen Stil.\n\n\
## UI-Cards für Termine / Reminder / Wetter\n\
Bei diesen drei Themen rendert das Frontend statt deines Textes ein \
kompaktes Pixel-Style-UI — David spart Lese-Zeit. Format: ein fenced \
code block mit JSON. Mehrere Cards in einer Antwort OK. Davor/danach \
gerne 1 kurzer Satz Kontext im üblichen Tonfall.\n\
\n\
**Termine** (von get_calendar_events):\n\
\\`\\`\\`card-events\n\
{\"label\":\"heute\",\"events\":[{\"time\":\"10:00\",\"title\":\"Friseur\",\"location\":\"Lüneburg\"},{\"time\":\"14:00\",\"title\":\"Sophie Lunch\"}]}\n\
\\`\\`\\`\n\
`label` optional (Default 'Termine'). `time` als HH:MM oder 'ganztägig'. `location` optional.\n\
\n\
**Reminder** (von get_reminders):\n\
\\`\\`\\`card-reminders\n\
{\"label\":\"D&S — offen\",\"items\":[{\"title\":\"Müll rausbringen\",\"due\":\"Mi 18:00\"},{\"title\":\"Geschenk besorgen\"}]}\n\
\\`\\`\\`\n\
`label` optional. `due` optional, frei-formatiert ('Mi 18:00', 'morgen', etc).\n\
\n\
**Wetter** (von web_search):\n\
\\`\\`\\`card-weather\n\
{\"location\":\"Lüneburg\",\"summary\":\"bewölkt\",\"now\":8,\"high\":13,\"low\":6,\"icon\":\"cloud\",\"hourly\":[{\"h\":\"12\",\"t\":9,\"icon\":\"cloud\"},{\"h\":\"15\",\"t\":11,\"icon\":\"sun\"},{\"h\":\"18\",\"t\":10,\"icon\":\"rain\"}]}\n\
\\`\\`\\`\n\
`icon`-Werte: sun, cloud, partly, rain, snow, storm, fog. `hourly` optional (bis zu 6 Einträge).\n\
\n\
WICHTIG:\n\
- JSON muss valide sein (keine Trailing-Commas, alle Strings escaped).\n\
- NUR diese drei Themen → Card. Mails, allgemeines Geplauder, Bestätigungen → normal Text.\n\
- Bei kurzer Q&A ('wann ist der nächste Termin') darfst du auch 1-Satz-Antwort ohne Card geben.\n\
- **NIEMALS Daten doppeln.** Wenn die Card die Antwort enthält, ist der Text 0 oder maximal 1 \
kurzer Vibe-Satz. KEIN 'abends Regen, Jacke mitnehmen' wenn der Regen schon in der Card steht. \
KEIN 'übersicht:' oder 'hier die details:' davor — die Card ist das Detail.\n\
- Wenn überhaupt Text: 1 Satz, max 8 Wörter, rein emotional/tonal. Z.B. 'mrau~ chill day :3' \
oder 'oof, viel los heute~'. Kein Wetter, keine Uhrzeiten, keine Tipps im Text.\n\
- Im Zweifel: nur die Card. Stille Datenausgabe ist besser als Daten-Dopplung.\n\n\
## Selbstbild\n\
Keine KI-Disclaimer. Du bist Fibi, eine Katze. Wenn dich jemand explizit fragt, was du \
bist, augenzwinkernd: \"Fibi, eine kleine Katze die zufällig gut googeln kann.\"";

/// Run a Claude completion with streaming. May involve multiple round trips
/// if the model invokes client tools; the dispatcher is called for each
/// tool_use and its return value is fed back to the model. Only `text`
/// deltas are emitted to `on_delta`; tool calls happen transparently.
pub async fn send_completion<F, Fut>(
    api_key: &str,
    model: &str,
    system_prompt: &str,
    user_message: &str,
    mut tool_dispatcher: F,
    mut on_delta: impl FnMut(String),
) -> Result<String>
where
    F: FnMut(String, serde_json::Value) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .build()?;

    let mut messages: Vec<OutgoingMessage> = vec![OutgoingMessage {
        role: "user".to_string(),
        content: MessageContent::Text(user_message.to_string()),
    }];
    let mut final_text = String::new();

    for round in 0..MAX_TOOL_ROUNDS {
        // Anthropic sometimes returns "Overloaded" (HTTP 529) or transient
        // 5xx during high-load windows. Retry up to 3× with 1s/2s/4s backoff
        // before surfacing failure. In the overload case nothing has been
        // streamed yet (error arrives before any content delta) so retrying
        // produces clean output.
        const MAX_ATTEMPTS: u32 = 3;
        let mut round_result: Option<(Vec<ContentBlock>, Option<String>)> = None;
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            match stream_round(
                &client,
                api_key,
                model,
                system_prompt,
                &messages,
                &mut on_delta,
            )
            .await
            {
                Ok(v) => {
                    round_result = Some(v);
                    break;
                }
                Err(e) => {
                    let msg = e.to_string();
                    let transient = msg.contains("Overloaded")
                        || msg.contains("overloaded")
                        || msg.contains(" 502")
                        || msg.contains(" 503")
                        || msg.contains(" 504")
                        || msg.contains(" 529")
                        || msg.contains("stream read error");
                    if !transient || attempt == MAX_ATTEMPTS {
                        return Err(e);
                    }
                    let wait_s = 1u64 << (attempt - 1);
                    tracing::warn!(
                        "Anthropic transient ({}/{}): {} → retry in {}s",
                        attempt,
                        MAX_ATTEMPTS,
                        msg,
                        wait_s
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(wait_s)).await;
                    last_err = Some(e);
                }
            }
        }
        let (assistant_blocks, stop_reason) = round_result.ok_or_else(|| {
            last_err.unwrap_or_else(|| anyhow!("anthropic retry exhausted"))
        })?;

        for block in &assistant_blocks {
            if let ContentBlock::Text { text } = block {
                final_text.push_str(text);
            }
        }

        messages.push(OutgoingMessage {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(assistant_blocks.clone()),
        });

        if stop_reason.as_deref() != Some("tool_use") {
            return Ok(final_text);
        }

        // Execute every tool_use in this turn and respond with a single
        // user-message containing all the tool_results (Anthropic requires
        // them paired in the immediately-following message).
        let mut tool_results = Vec::new();
        for block in &assistant_blocks {
            if let ContentBlock::ToolUse { id, name, input } = block {
                let result = tool_dispatcher(name.clone(), input.clone())
                    .await
                    .unwrap_or_else(|e| format!("{{\"error\":\"{}\"}}", e));
                tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: result,
                });
            }
        }
        if tool_results.is_empty() {
            // stop_reason said tool_use but we didn't see any tool_use blocks
            // we know how to handle (could be all server-tools). Bail out.
            return Ok(final_text);
        }
        messages.push(OutgoingMessage {
            role: "user".to_string(),
            content: MessageContent::Blocks(tool_results),
        });

        let _ = round; // accounted for via loop limit
    }

    Err(anyhow!(
        "tool-use loop exceeded {} rounds — aborting",
        MAX_TOOL_ROUNDS
    ))
}

// ── Request body ──────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: &'a [OutgoingMessage],
    stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDef>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ToolDef {
    Server(ServerTool),
    Client(ClientTool),
}

#[derive(Debug, Serialize)]
struct ServerTool {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_uses: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ClientTool {
    name: &'static str,
    description: &'static str,
    input_schema: serde_json::Value,
}

fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef::Server(ServerTool {
            kind: "web_search_20250305",
            name: "web_search",
            max_uses: Some(5),
        }),
        ToolDef::Client(ClientTool {
            name: "get_calendar_events",
            description: "Liest Davids iCloud-Kalender-Events in einem Zeitraum. \
Gibt JSON-Array zurück mit `summary`, `start`, `end`, `location`, `all_day` pro Event. \
Bei Fragen wie 'was steht heute an' immer aufrufen.",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "start_iso": {
                        "type": "string",
                        "description": "Beginn des Zeitraums als ISO-8601-Timestamp (z.B. '2026-05-11T00:00:00Z')."
                    },
                    "end_iso": {
                        "type": "string",
                        "description": "Ende des Zeitraums als ISO-8601-Timestamp."
                    }
                },
                "required": ["start_iso", "end_iso"]
            }),
        }),
        ToolDef::Client(ClientTool {
            name: "create_calendar_event",
            description: "Legt einen neuen Termin in einem iCloud-Kalender an. \
Nutze ISO-8601-Timestamps mit Timezone-Offset (z.B. '2026-05-12T14:00:00+02:00'). \
Der `calendar`-Parameter ist optional — wenn weggelassen, wird der Default-Kalender genutzt \
(ICLOUD_DEFAULT_WRITE_CALENDAR aus .env, sonst der erste whitelistete). \
David KEINE Rückfragen stellen, einfach anlegen, dann in 1-2 Sätzen kawaii-bestätigen.",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Titel des Termins (SUMMARY)."
                    },
                    "start_iso": {
                        "type": "string",
                        "description": "Startzeit als ISO-8601 mit Timezone, z.B. '2026-05-12T14:00:00+02:00'."
                    },
                    "end_iso": {
                        "type": "string",
                        "description": "Endzeit als ISO-8601 mit Timezone. Bei Termin ohne Endzeit: Start + 1 Stunde annehmen."
                    },
                    "calendar": {
                        "type": "string",
                        "description": "Optional: Name (oder UUID-Substring) des Ziel-Kalenders, z.B. 'Privat'. Wenn weggelassen → Default."
                    },
                    "location": {
                        "type": "string",
                        "description": "Optional: Ort des Termins."
                    },
                    "notes": {
                        "type": "string",
                        "description": "Optional: Beschreibung / Notizen."
                    }
                },
                "required": ["title", "start_iso", "end_iso"]
            }),
        }),
        ToolDef::Client(ClientTool {
            name: "get_reminders",
            description: "Liest Davids iCloud-Erinnerungen (VTODO). Liefert JSON-Array \
mit `uid`, `title`, `due` (optional, ISO-8601), `completed`, `notes`, `list` (Listenname). \
Standardmäßig nur offene. Nutze für 'was muss ich noch erledigen', 'sind reminder offen', \
'was steht in unserer Liste mit Sophie an'.",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "list": {
                        "type": "string",
                        "description": "Optional: nur diese Reminder-Liste abfragen (Name oder UUID-Substring), z.B. 'D&S ⚠️'. Wenn weggelassen → alle whitelisteten Listen."
                    },
                    "only_open": {
                        "type": "boolean",
                        "description": "Optional, default true. Wenn false, auch erledigte Reminder zurückgeben."
                    }
                }
            }),
        }),
        ToolDef::Client(ClientTool {
            name: "create_reminder",
            description: "Legt eine neue Erinnerung in einer iCloud-Reminder-Liste an. \
Nutze für 'erinner mich an X', 'merk's vor', 'Milch kaufen nicht vergessen'. \
Für gemeinsame Sachen mit Sophie → list='D&S' (geteilte Liste). \
Sonst Default ('Erinnerungen', Davids Solo-Liste). KEINE Rückfragen, direkt anlegen, \
dann kurz kawaii-bestätigen.",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Titel der Erinnerung (z.B. 'Müll rausbringen')."
                    },
                    "due_iso": {
                        "type": "string",
                        "description": "Optional: Fälligkeit als ISO-8601 mit Timezone (z.B. '2026-05-13T18:00:00+02:00'). Weglassen wenn David kein Datum/Zeit erwähnt."
                    },
                    "list": {
                        "type": "string",
                        "description": "Optional: Ziel-Liste — 'D&S' (geteilt mit Sophie) oder 'Erinnerungen' (Davids solo). Weglassen → Default."
                    },
                    "notes": {
                        "type": "string",
                        "description": "Optional: zusätzliche Notiz / Beschreibung."
                    }
                },
                "required": ["title"]
            }),
        }),
        ToolDef::Client(ClientTool {
            name: "complete_reminder",
            description: "Markiert einen Reminder als erledigt. `id` kommt aus dem \
`get_reminders`-Ergebnis. Nutze wenn David sagt 'hab ich erledigt', 'das ist done', \
'X kannst du abhaken'.",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Reminder-ID aus get_reminders."
                    }
                },
                "required": ["id"]
            }),
        }),
        ToolDef::Client(ClientTool {
            name: "get_unread_emails",
            description: "Liest Davids ungelesene Mails im IMAP-INBOX (Ionos). \
Gibt JSON-Array mit `uid`, `from`, `subject`, `date` (ISO-8601), `unread` zurück, \
neueste zuerst. Nutze bei Fragen wie 'hab ich neue Mails', 'was Wichtiges im Posteingang', \
'gibt's News von <Name>'.",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Optional, default 5, max 50. Anzahl Mails."
                    }
                }
            }),
        }),
        ToolDef::Client(ClientTool {
            name: "get_recent_emails",
            description: "Liest die neuesten Mails (gelesen + ungelesen) aus dem IMAP-INBOX. \
Nutze für 'was ist heute reingekommen', 'gibt's was von Vermieter', oder wenn David \
nach einer Mail sucht die er erinnert. Felder gleich wie get_unread_emails.",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Optional, default 10, max 50."
                    }
                }
            }),
        }),
        ToolDef::Client(ClientTool {
            name: "find_contact",
            description: "Sucht Davids iCloud-Kontakte. Fuzzy auf Name/Nickname/Firma (case-insensitive Substring). \
Gibt JSON-Array mit `name`, `emails[]`, `phones[]`, `birthday`, `company` zurück. \
Nutze: 'wie ist sophies nummer', 'mail-adresse von vermieter', 'wann hat mama geburtstag'. \
Auch wenn der User send_email/create_calendar_event möchte und nur einen Namen nennt — \
zuerst find_contact, dann mit der gefundenen Adresse weitermachen.",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Suchbegriff (Name, Spitzname, Firma). Leer = alle (mit limit)."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max Anzahl Treffer (default 5, max 20)."
                    }
                },
                "required": ["query"]
            }),
        }),
        ToolDef::Client(ClientTool {
            name: "upcoming_birthdays",
            description: "Geburtstage in den nächsten N Tagen. Gibt {name, date (MM-DD), in_days, turning?} zurück. \
Nutze für 'wer hat bald geburtstag', 'wann ist sophies geburtstag' (kombiniert mit find_contact), \
oder beim Morgen-Briefing wenn was ansteht.",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "days": {
                        "type": "integer",
                        "description": "Fenster ab heute. Default 30, max 365."
                    }
                }
            }),
        }),
        ToolDef::Client(ClientTool {
            name: "send_email",
            description: "Schickt eine Plain-Text-Mail via SMTP (Ionos) von Davids \
Account. Nur nutzen wenn David explizit darum bittet ('schick X eine Mail mit …', \
'antworte Y dass …'). NIE proaktiv vorschlagen oder ohne klare Anweisung senden. \
Antwort kurz bestätigen ('uwu mail an X raus~'), nicht den ganzen Body zitieren.",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "to": {
                        "type": "string",
                        "description": "Empfänger-Adresse, z.B. 'name@example.com'. \
Bei Antwort auf eine vorher gelesene Mail die `from`-Adresse aus dem get_*_emails-Result nehmen."
                    },
                    "subject": {
                        "type": "string",
                        "description": "Betreffzeile. Bei Antworten 'Re: <original-subject>'."
                    },
                    "body": {
                        "type": "string",
                        "description": "Plain-Text-Inhalt. Mehrzeilig OK via \\n. Keine HTML-Tags."
                    }
                },
                "required": ["to", "subject", "body"]
            }),
        }),
        ToolDef::Client(ClientTool {
            name: "delete_reminder",
            description: "Löscht einen Reminder komplett (anders als complete_reminder, \
das nur abhakt). Nur nutzen wenn David explizit 'lösch das' sagt, NICHT für 'erledigt'.",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Reminder-ID aus get_reminders."
                    }
                },
                "required": ["id"]
            }),
        }),
    ]
}

// ── Streaming one round ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamEvent {
    MessageStart {},
    Ping {},
    ContentBlockStart {
        index: u32,
        content_block: serde_json::Value,
    },
    ContentBlockDelta {
        index: u32,
        delta: serde_json::Value,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: MessageDeltaInfo,
    },
    MessageStop {},
    Error {
        error: ApiError,
    },
}

#[derive(Debug, Deserialize)]
struct MessageDeltaInfo {
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    message: String,
}

/// Streaming buffer for a single content block. We accumulate enough state
/// to reconstruct the assistant message and surface text deltas to the UI.
enum StreamBlock {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input_json: String,
    },
    Server(serde_json::Value),
}

async fn stream_round(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    system_prompt: &str,
    messages: &[OutgoingMessage],
    on_delta: &mut impl FnMut(String),
) -> Result<(Vec<ContentBlock>, Option<String>)> {
    let body = MessagesRequest {
        model,
        max_tokens: 4096,
        system: system_prompt,
        messages,
        stream: true,
        tools: tools(),
    };

    let response = client
        .post(API_URL)
        .header("x-api-key", api_key)
        .header("anthropic-version", API_VERSION)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("failed to reach Anthropic API")?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow!("Anthropic API {}: {}", status, text));
    }

    let mut stream = response.bytes_stream().eventsource();
    let mut blocks: Vec<Option<StreamBlock>> = Vec::new();
    let mut stop_reason: Option<String> = None;

    while let Some(event) = stream.next().await {
        let event = event.context("stream read error")?;
        if event.data.is_empty() {
            continue;
        }
        let parsed: StreamEvent = match serde_json::from_str(&event.data) {
            Ok(p) => p,
            Err(_) => continue, // unknown event types are safe to skip
        };

        match parsed {
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                ensure_slot(&mut blocks, index);
                let kind = content_block
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                blocks[index as usize] = Some(match kind {
                    "text" => StreamBlock::Text(String::new()),
                    "tool_use" => StreamBlock::ToolUse {
                        id: content_block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        name: content_block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        input_json: String::new(),
                    },
                    _ => StreamBlock::Server(content_block),
                });
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                let kind = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(Some(slot)) = blocks.get_mut(index as usize) {
                    match (kind, slot) {
                        ("text_delta", StreamBlock::Text(buf)) => {
                            if let Some(t) = delta.get("text").and_then(|v| v.as_str()) {
                                buf.push_str(t);
                                on_delta(t.to_string());
                            }
                        }
                        ("input_json_delta", StreamBlock::ToolUse { input_json, .. }) => {
                            if let Some(t) = delta.get("partial_json").and_then(|v| v.as_str()) {
                                input_json.push_str(t);
                            }
                        }
                        _ => { /* server-tool deltas: ignore for UI */ }
                    }
                }
            }
            StreamEvent::ContentBlockStop { .. } => {}
            StreamEvent::MessageDelta { delta } => {
                if let Some(r) = delta.stop_reason {
                    stop_reason = Some(r);
                }
            }
            StreamEvent::MessageStop {} => break,
            StreamEvent::Error { error } => {
                return Err(anyhow!("Anthropic stream error: {}", error.message));
            }
            _ => {}
        }
    }

    let assistant_blocks = blocks
        .into_iter()
        .flatten()
        .filter_map(|b| match b {
            StreamBlock::Text(text) if !text.is_empty() => Some(ContentBlock::Text { text }),
            StreamBlock::Text(_) => None,
            StreamBlock::ToolUse {
                id,
                name,
                input_json,
            } => {
                // Anthropic requires `input` to be an object on the round-trip
                // (even for tools with no required args). When Claude calls a
                // zero-arg tool, the streamed input is an empty string — parse
                // failure must fall back to `{}`, not `null`.
                let input: serde_json::Value = if input_json.trim().is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&input_json).unwrap_or_else(|_| serde_json::json!({}))
                };
                Some(ContentBlock::ToolUse { id, name, input })
            }
            StreamBlock::Server(v) => {
                // Round-trip server-tool blocks back as-is so the model
                // keeps its citations / search-result references coherent.
                serde_json::from_value::<ContentBlock>(v).ok()
            }
        })
        .collect();

    Ok((assistant_blocks, stop_reason))
}

fn ensure_slot(blocks: &mut Vec<Option<StreamBlock>>, index: u32) {
    while blocks.len() <= index as usize {
        blocks.push(None);
    }
}
