# Presence

What slaq does about presence — and, more usefully, what it **cannot** do.

## Slack's model

- `users.setPresence` accepts `auto` or `away`.
  - `away` is a **manual** away: it persists across connections and overrides
    Slack's automatic rules.
  - `auto` **clears a manual away** and hands the decision back to Slack. It is
    **not** a force-active: there is no call that makes a user active.
- Whether someone shows as active is Slack's decision, from a client that is
  connected and not idle (Slack marks an idle client away after a while).

So "make me active" is not something an API call can do; "clear anything that is
holding me away" is.

## What slaq does

- **Asserts `auto`** for as long as the app runs: on the first authenticated frame
  (which clears a manual away left over from an earlier session) and then as a
  keepalive every two minutes. It is deliberately **not** gated on the window
  having focus — working in another window is not being away, and gating on focus
  is exactly what made the account drop offline.
- **Never goes away by itself.** The only `away` is the explicit choice in the
  profile popover; while that is set the keepalive stands down, and the popover
  keeps showing the user's choice rather than the server's transient verdict.
- **Reads the result back.** After each assert it calls `users.getPresence` and
  logs `presence -> auto, now <value>`; an incoming `presence_change` for ourselves
  is logged too (we subscribe to our own id). The behaviour is therefore
  observable, not assumed.

## Why the read-back matters

If the account still shows offline for colleagues, the log says which case it is:

- `presence -> auto, now active` and they still see offline → the call is not the
  problem (look at the workspace, or another client overriding it).
- `presence -> auto, now away` → `auto` did not clear it: it is Slack's own idle
  away, which **no API can override**. That is a limitation to state, not a bug to
  patch blindly.

## Not used

`users.setActive` (deprecated), a second websocket, and the client-side activity
signals the official clients send.
