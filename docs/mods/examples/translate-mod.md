# Worked example — an automatic-translation mod

This walks an incoming-translation mod end to end, using **only** v0
capabilities (see [`manifest-v0.md`](../manifest-v0.md)). It doubles as a
validation of the capability list: writing it surfaced one missing primitive
(`ui.message_decoration`), now part of v0.

## Manifest

`com.example.translate`, version 0.1.0:

```json
{
  "manifestVersion": 0,
  "id": "com.example.translate",
  "name": "Auto Translate",
  "version": "0.1.0",
  "apiVersion": "0.1",
  "minSlaq": "0.4.0",
  "entry": { "type": "wasm", "path": "mod.wasm" },
  "locales": ["en", "it"],

  "capabilities": {
    "messages.read":         { "scope": "channels", "channels": ["C0ENGINEER", "D0ME"] },
    "store.kv":              { "maxBytes": 2097152 },
    "net.egress":            ["localhost:11434"],
    "ui.message_action":     {},
    "ui.message_decoration": {},
    "ui.settings":           {}
  },
  "events": ["message.created", "message.updated"],
  "ui": {
    "messageActions": [
      { "id": "show-original", "label": "i18n:show_original" },
      { "id": "retranslate",   "label": "i18n:retranslate" }
    ]
  },
  "settings": [
    { "key": "targetLang", "type": "string", "default": "it", "label": "i18n:target_lang" },
    { "key": "mode",       "type": "enum",   "default": "auto",
      "values": ["auto", "on_demand"], "label": "i18n:mode" },
    { "key": "endpoint",   "type": "url",    "default": "http://localhost:11434/v1" },
    { "key": "apiKey",     "type": "secret", "label": "API key" }
  ]
}
```

The default endpoint is **local**, so nothing leaves the machine unless the user
opts into a remote provider.

## Install

The host renders the consent prompt from the manifest:

> **Auto Translate** wants to:
> ☑ Read messages — **only**: #engineer, DM with you
> ☑ Contact: `localhost:11434`
> ☑ Store data (up to 2 MiB) — translation cache, settings
> ☑ Add message items and a settings panel
>
> ⚠️ Read messages + contact an external host = content can leave the machine.

The last line is the **dangerous-combination** warning. Anything not checked is
not callable.

## Start

The host instantiates the component and calls the exported `handler.start`
(it returns `result<_, error>`, nothing else). To learn what was granted the mod
calls the imported `grants.list`. From here it is events only.

## A message arrives

```
Ciao <@U123>, vedi <https://x.com/y|questo> :rocket:
```

Because the channel is in scope and the mod subscribed to `message.created`,
the host pushes **metadata**, not content:

```json
{ "event": "message.created", "channel": "C0ENGINEER",
  "ts": "1712345678.000100", "author": "U999" }
```

The mod fetches the text:

```json
{ "call": "messages.get",
  "refs": [{ "channel": "C0ENGINEER", "ts": "1712345678.000100" }] }
```

The host checks the scope (channel ok) and returns the raw mrkdwn. A request for
a channel outside the scope returns `permission-denied` — the scope is real.

Two `store.kv` reads: the cached translation (`tr:…`) and the detected language
(`lang:…`). If it is already in the target language, stop.

## Translation (the delicate part)

The mod does **not** send raw text to the engine: that would destroy mentions,
links and emoji. It replaces structural tokens with placeholders:

```
Ciao ⟦0⟧, vedi ⟦1⟧ ⟦2⟧   →  { "0":"<@U123>", "1":"<https://x.com/y|questo>", "2":":rocket:" }
```

Then calls the engine (here, local):

```json
{ "call": "http.fetch",
  "req": { "method": "POST", "url": "http://localhost:11434/v1/chat/completions",
           "body": { "…": "translate to it, keep ⟦n⟧ intact" } } }
```

For a remote provider the mod would declare that host in `net.egress` and add a
`secret` setting; the host stores the key and the mod reads it (labelled). The
mod reassembles:

```
Ciao <@U123>, guarda <https://x.com/y|questo> :rocket:
```

and caches it: `tr:C0ENGINEER:1712345678.000100:it:local`.

> ⚠️ The engine's output is **untrusted input**. Before reassembling, the mod
> requires the output's placeholders to be **exactly** the input's multiset (no
> missing, duplicated or unknown `⟦n⟧`) **and** rejects any structural mrkdwn the
> engine's text contains outside those placeholders — a literal `<@U…>`, `<#C…>`,
> `<url|…>`, `<…>` or `:emoji:` it invented. Anything else is discarded and the
> original kept, so a malicious engine can neither duplicate a mention/link nor
> inject new structural tokens.

## Rendering

The mod cannot draw, so it attaches a declarative decoration:

```json
{ "call": "ui.set_message_decoration",
  "ref": { "channel": "C0ENGINEER", "ts": "1712345678.000100" },
  "content": [
    { "type": "context", "text": "🇮🇹 translated · local" },
    { "type": "section", "text": "Ciao <@U123>, guarda <https://x.com/y|questo> :rocket:" }
  ] }
```

The host renders it under the message in egui **and** ratatui. It is **local and
ephemeral**: not a Slack message, not in history, invisible to others.

## Interaction

The declared message actions appear on hover. A click yields:

```json
{ "event": "action.invoked", "actionId": "show-original",
  "channel": "C0ENGINEER", "ts": "1712345678.000100", "user": "U777" }
```

The mod clears or toggles the decoration (`ui.clear_message_decoration`), or
re-reads from KV.

## Variant: outgoing translation

To "write in Italian, send in English", the same mod adds the
`ui.composer_action` capability, a `ui.composerActions` entry in the manifest
(`{ "id": "translate-send", "label": "i18n:translate_send" }`) and `chat.send`.
It never rewrites silently:

1. The user clicks the composer action; the host sends
   `action.invoked { actionId, composerDraft }`.
2. The mod translates and returns a **preview**:
   `ui.set_composer_preview { content, actions:[{id:"send-translated"}] }`.
3. The user clicks "Send translated"; the host issues a fresh `action.invoked`
   carrying an activation token bound to this send, and the mod calls
   `chat.send(token, channel, translatedText)`.

> **Slice-note:** `chat.send` ships **ambient** (no activation token, no preview),
> and `ui.composer_action` ships with `ui-set-composer-text` (the composer
> **becomes** the proposed text; the user sends it) — the **preview + inline send**
> flow above (`ui.set-composer-preview`) is the target and still to land.

The alternative is `ui.set_composer_text`: the composer becomes the translation
and the user presses Enter themselves. Either way, the text that goes out is
seen first.

## Errors and boundaries

- Engine down → no decoration, retry, short negative cache.
- Message edited (`message.updated`) → invalidate cache, retranslate.
- Malformed engine output → discarded.
- Missing API key → degrade to local or surface it via `ui.settings`.

## Capabilities exercised

| Capability | Use |
|---|---|
| `messages.read` (scope) | read message content |
| `net.egress` | local/remote engine |
| `store.kv` | translation cache, detected language, settings |
| events `message.created` / `message.updated` | auto update, retranslate on edit |
| `ui.message_action` | show-original / retranslate |
| `ui.message_decoration` | **surfaced by this exercise** |
| `ui.settings` | target language, mode, engine |
| `ui.composer_action` + `chat.send` | outgoing variant |

## What the exercise taught

1. v0 holds, with one addition: `ui.message_decoration` is required for anything
   attached to a message.
2. "Events = metadata, content via `messages.get`" makes the **scope** real.
3. A local default endpoint keeps the privacy promise; a remote provider is an
   explicit opt-in with its own warning.
4. mrkdwn segmentation is the easiest place for a mod to go wrong — a host
   helper (`text.segments` → translatable segments + structural tokens) would
   remove duplicated logic from every translation mod. Open question.
