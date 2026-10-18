#!/usr/bin/env python3
"""Render candidate dark palettes for slaq as a single style tile.

Pure stdlib: emits an HTML file next to this script (or --out PATH) that shows,
for each candidate identity, a realistic mock of the GUI (sidebar, conversation,
composer) plus the role swatches and measured WCAG contrast. Rendering the HTML
to PNG is a separate step (chromium --headless --screenshot).

This is a decision aid: it exists so the palette can be chosen by eye before any
Rust is touched.
"""

from __future__ import annotations

import argparse
import pathlib

# --- WCAG -------------------------------------------------------------------


def _srgb_to_lin(c: float) -> float:
    return c / 12.92 if c <= 0.04045 else ((c + 0.055) / 1.055) ** 2.4


def luminance(hex_color: str) -> float:
    h = hex_color.lstrip("#")
    r, g, b = (int(h[i : i + 2], 16) / 255.0 for i in (0, 2, 4))
    return 0.2126 * _srgb_to_lin(r) + 0.7152 * _srgb_to_lin(g) + 0.0722 * _srgb_to_lin(b)


def contrast(fg: str, bg: str) -> float:
    l1, l2 = luminance(fg), luminance(bg)
    hi, lo = max(l1, l2), min(l1, l2)
    return (hi + 0.05) / (lo + 0.05)


# --- Candidates -------------------------------------------------------------

CANDIDATES = [
    {
        "key": "A",
        "name": "Slate + Signal Indigo",
        "tag": "Calm, corporate, familiar. Indigo reads “software / trust”,\nand keeps a clear distance from Slack’s aubergine-green.",
        "roles": {
            "bg_base": "#0B0E14",
            "bg_panel": "#090C12",
            "bg_elevated": "#161B24",
            "bg_input": "#161B24",
            "bg_hover": "#1C2230",
            "bg_selected": "#213156",
            "border_subtle": "#202836",
            "border_strong": "#2E3A4D",
            "text_primary": "#E6EAF2",
            "text_secondary": "#9AA6B8",
            "text_muted": "#6B7686",
            "accent": "#6E8BFF",
            "accent_fg": "#08122B",
            "link": "#79B8FF",
            "success": "#3FB950",
            "warning": "#D9A441",
            "danger": "#E5534B",
            "mention_self_bg": "#E5B84B",
            "mention_self_fg": "#1A1508",
            "badge_unread_bg": "#E6EAF2",
            "badge_unread_fg": "#0B0E14",
            "badge_mention_bg": "#E5534B",
            "badge_mention_fg": "#FFFFFF",
            "presence_active": "#3FB950",
            "presence_away": "#6B7686",
            "presence_quiet": "#8C8CE0",
            "reaction_bg": "#1C2230",
        },
    },
    {
        "key": "B",
        "name": "Graphite + Teal",
        "tag": "Productivity-tool calm. A cool teal accent on warm graphite\nis distinctive without shouting; reads “measured, operational”.",
        "roles": {
            "bg_base": "#101312",
            "bg_panel": "#0B0E0D",
            "bg_elevated": "#1A1E1D",
            "bg_input": "#1A1E1D",
            "bg_hover": "#242A28",
            "bg_selected": "#153B37",
            "border_subtle": "#242A28",
            "border_strong": "#333B38",
            "text_primary": "#E8ECEA",
            "text_secondary": "#9BA8A3",
            "text_muted": "#6E7B76",
            "accent": "#2FB6A3",
            "accent_fg": "#04201C",
            "link": "#5FA8E0",
            "success": "#57C46E",
            "warning": "#D8A24A",
            "danger": "#E5605A",
            "mention_self_bg": "#E5B84B",
            "mention_self_fg": "#1A1508",
            "badge_unread_bg": "#E8ECEA",
            "badge_unread_fg": "#101312",
            "badge_mention_bg": "#E5605A",
            "badge_mention_fg": "#FFFFFF",
            "presence_active": "#57C46E",
            "presence_away": "#6E7B76",
            "presence_quiet": "#8FA0E0",
            "reaction_bg": "#242A28",
        },
    },
    {
        "key": "C",
        "name": "Navy + Amber",
        "tag": "Warm and unmistakably ours. Deep navy with a gold accent is\nrare in chat clients; the boldest of the three.",
        "roles": {
            "bg_base": "#0B1222",
            "bg_panel": "#070C17",
            "bg_elevated": "#14203A",
            "bg_input": "#14203A",
            "bg_hover": "#1B2A48",
            "bg_selected": "#263B66",
            "border_subtle": "#1B2740",
            "border_strong": "#2C3B5C",
            "text_primary": "#E9EEF9",
            "text_secondary": "#9DAAC6",
            "text_muted": "#6E7B99",
            "accent": "#E0A030",
            "accent_fg": "#1A1200",
            "link": "#7FB3FF",
            "success": "#3FBF7F",
            "warning": "#E8743B",
            "danger": "#E5534B",
            "mention_self_bg": "#E0A030",
            "mention_self_fg": "#1A1200",
            "badge_unread_bg": "#E9EEF9",
            "badge_unread_fg": "#0B1222",
            "badge_mention_bg": "#E5534B",
            "badge_mention_fg": "#FFFFFF",
            "presence_active": "#3FBF7F",
            "presence_away": "#6E7B99",
            "presence_quiet": "#8C8CE0",
            "reaction_bg": "#1B2A48",
        },
    },
]

# (label, fg-role, bg-role, needed) — needed: 4.5 body, 3.0 large/UI
CONTRAST_CHECKS = [
    ("body on base", "text_primary", "bg_base", 4.5),
    ("secondary on base", "text_secondary", "bg_base", 4.5),
    ("muted on base", "text_muted", "bg_base", 4.5),
    ("body on elevated", "text_primary", "bg_elevated", 4.5),
    ("on accent", "accent_fg", "accent", 4.5),
    ("link on base", "link", "bg_base", 3.0),
    ("mention fg/bg", "mention_self_fg", "mention_self_bg", 4.5),
    ("unread badge", "badge_unread_fg", "badge_unread_bg", 4.5),
    ("mention badge", "badge_mention_fg", "badge_mention_bg", 4.5),
]

SWATCH_ORDER = [
    "bg_base", "bg_panel", "bg_elevated", "bg_hover", "bg_selected",
    "border_subtle", "border_strong", "text_primary", "text_secondary",
    "text_muted", "accent", "link", "success", "warning", "danger",
    "mention_self_bg", "badge_unread_bg", "badge_mention_bg",
    "presence_active", "presence_quiet",
]


def _badge(ratio: float, needed: float) -> str:
    ok = ratio >= needed
    cls = "ok" if ok else ("warn" if ratio >= needed - 0.5 else "bad")
    mark = "PASS" if ok else "FAIL"
    return f'<span class="ctr {cls}">{ratio:.2f} · {mark} (min {needed:g})</span>'


def render_candidate(c: dict) -> str:
    r = c["roles"]
    sw = "".join(
        f'<div class="sw"><span class="chip" style="background:{r[k]}"></span>'
        f'<span class="swk">{k}</span><span class="swv">{r[k]}</span></div>'
        for k in SWATCH_ORDER
    )
    ctr = "".join(
        f'<div class="ctrow"><span class="ctl">{label}</span>'
        f'{_badge(contrast(r[fg], r[bg]), need)}</div>'
        for label, fg, bg, need in CONTRAST_CHECKS
    )
    return f"""
    <section class="cand" style="
        --bg:{r['bg_base']}; --panel:{r['bg_panel']}; --elev:{r['bg_elevated']};
        --hov:{r['bg_hover']}; --sel:{r['bg_selected']}; --bsub:{r['border_subtle']};
        --bstr:{r['border_strong']}; --tp:{r['text_primary']}; --ts:{r['text_secondary']};
        --tm:{r['text_muted']}; --acc:{r['accent']}; --accfg:{r['accent_fg']};
        --link:{r['link']}; --ok:{r['success']}; --warn:{r['warning']}; --dang:{r['danger']};
        --men:{r['mention_self_bg']}; --menfg:{r['mention_self_fg']};
        --ubg:{r['badge_unread_bg']}; --ufg:{r['badge_unread_fg']};
        --mbg:{r['badge_mention_bg']}; --mfg:{r['badge_mention_fg']};
        --act:{r['presence_active']}; --away:{r['presence_away']}; --quiet:{r['presence_quiet']};
        --reac:{r['reaction_bg']}">
      <header class="chead">
        <div class="ckey">{c['key']}</div>
        <div>
          <h2>{c['name']}</h2>
          <p>{c['tag']}</p>
        </div>
        <div class="acccard"><span></span><b>{r['accent']}</b></div>
      </header>

      <div class="app">
        <div class="topbar">
          <span class="brand">slaq</span>
          <span class="top-right"><span class="dotmini" style="background:var(--ok)"></span>online<span class="gear"></span></span>
        </div>
        <div class="cbody">
          <aside class="side">
            <div class="ws">DangerBlack ▾</div>
            <div class="search">Cerca</div>
            <div class="lbl">Canali</div>
            <div class="row"><span class="hash">#</span> general</div>
            <div class="row active"><span class="hash">#</span> design-system <span class="bdg mention">3</span></div>
            <div class="row unread"><span class="hash">#</span> product <span class="bdg">12</span></div>
            <div class="row"><span class="hash">#</span> random</div>
            <div class="lbl">Messaggi diretti</div>
            <div class="row"><span class="pres" style="background:var(--act)"></span> Anna Ferrari</div>
            <div class="row"><span class="pres ring" style="--away:var(--away)"></span> Marco Rossi</div>
            <div class="row"><span class="pres" style="background:var(--quiet)"></span> Giulia Bianchi</div>
          </aside>
          <main class="chan">
            <div class="chanhead"><b># design-system</b><span class="sub">Palette e token di slaq</span></div>
            <div class="msgs">
              <div class="msg"><span class="av" style="background:var(--sel);color:var(--ts)">AF</span>
                <div><div class="meta"><b>Anna Ferrari</b><span class="time">10:24</span></div>
                <div class="txt">Bozza dark pronta, link di riferimento <span class="lnk">figma.com/slaq</span></div></div></div>
              <div class="msg"><span class="av" style="background:var(--reac);color:var(--ts)">MR</span>
                <div><div class="meta"><b>Marco Rossi</b><span class="time">10:26</span></div>
                <div class="txt"><span class="men">@daniele</span> uso <code>--color-accent</code> per il focus e gli stati attivi, ok?</div>
                <div class="reacs"><span class="reac">👍 2</span><span class="reac">🎨 1</span></div></div></div>
              <div class="divider"><span>nuovi messaggi</span></div>
              <div class="msg"><span class="av" style="background:var(--hov);color:var(--ts)">GB</span>
                <div><div class="meta"><b>Giulia Bianchi</b><span class="time">10:31</span></div>
                <div class="txt">Contrasto verificato, e i badge non sono più <span class="warn">ambigui</span>.</div>
                <div class="thread">↳ 3 risposte <span class="time">ultima 10:29</span></div></div></div>
            </div>
            <div class="composer"><span class="ph">Scrivi a #design-system…</span>
              <span class="cbtn">＋</span><span class="cbtn acc">➤</span></div>
          </main>
        </div>
      </div>

      <div class="swatches">{sw}</div>
      <div class="ctrbox"><div class="cttitle">Contrasto WCAG (calcolato)</div>{ctr}</div>
    </section>"""


def render_page() -> str:
    cands = "".join(render_candidate(c) for c in CANDIDATES)
    return f"""<!doctype html>
<html lang="it"><head><meta charset="utf-8"><title>slaq — palette dark</title>
<style>
  :root {{ color-scheme: dark; }}
  * {{ box-sizing: border-box; }}
  body {{ margin:0; background:#05070b; color:#e9eef9;
    font-family:"Inter","Noto Sans",system-ui,-apple-system,sans-serif; }}
  .page {{ padding:26px 26px 34px; }}
  h1 {{ font-size:26px; margin:0 0 2px; letter-spacing:-0.02em; }}
  .lead {{ color:#8b97ab; font-size:13.5px; margin:0 0 22px; max-width:1100px; line-height:1.5; }}
  .grid {{ display:grid; grid-template-columns:repeat(3, 1fr); gap:22px; align-items:start; }}
  .cand {{ border:1px solid #1a2130; border-radius:16px; padding:16px; background:#0a0d14; }}
  .chead {{ display:grid; grid-template-columns:auto 1fr auto; gap:12px; align-items:start; margin-bottom:14px; }}
  .ckey {{ width:30px;height:30px;border-radius:9px;background:#151b26;border:1px solid #26303f;
    display:grid;place-items:center;font-weight:700;color:#c9d4e6; }}
  .chead h2 {{ font-size:16px; margin:0 0 4px; }}
  .chead p {{ font-size:11.5px; color:#8695ab; margin:0; line-height:1.45; white-space:pre-line; }}
  .acccard {{ display:flex;flex-direction:column;align-items:center;gap:5px;font-size:10px;color:#8695ab; }}
  .acccard span {{ width:34px;height:34px;border-radius:9px;background:var(--acc);display:block; }}
  .app {{ border:1px solid #1a2130; border-radius:12px; overflow:hidden; }}
  .topbar {{ background:var(--panel); border-bottom:1px solid var(--bsub); display:flex;
    justify-content:space-between; align-items:center; padding:7px 11px; font-size:12px; }}
  .brand {{ font-weight:800; letter-spacing:-0.02em; }}
  .top-right {{ display:flex;align-items:center;gap:6px;color:var(--tm);font-size:10.5px; }}
  .dotmini {{ width:8px;height:8px;border-radius:50%; }}
  .gear {{ width:12px;height:12px;border-radius:50%;border:2px solid var(--tm);margin-left:8px; }}
  .cbody {{ display:grid; grid-template-columns:150px 1fr; min-height:322px; background:var(--bg); }}
  .side {{ background:var(--panel); border-right:1px solid var(--bsub); padding:8px 6px; font-size:11.5px; }}
  .ws {{ font-weight:700; padding:4px 6px 8px; }}
  .search {{ background:var(--elev); border:1px solid var(--bsub); color:var(--tm);
    border-radius:7px; padding:4px 7px; margin-bottom:8px; font-size:10.5px; }}
  .lbl {{ color:var(--tm); font-size:9px; text-transform:uppercase; letter-spacing:0.08em;
    padding:6px 6px 3px; }}
  .row {{ display:flex; align-items:center; gap:6px; padding:3.5px 7px; border-radius:7px; color:var(--ts); }}
  .row.active {{ background:var(--bg-selected,var(--sel)); color:var(--tp); font-weight:600; }}
  .row.unread {{ color:var(--tp); font-weight:700; }}
  .hash {{ color:var(--tm); }}
  .bdg {{ margin-left:auto; background:var(--ubg); color:var(--ufg); border-radius:9px;
    padding:0 6px; font-size:9.5px; font-weight:800; line-height:15px; }}
  .bdg.mention {{ background:var(--mbg); color:var(--mfg); }}
  .pres {{ width:8px;height:8px;border-radius:50%; flex:0 0 8px; }}
  .pres.ring {{ background:transparent; border:1.6px solid var(--away); }}
  .chan {{ display:flex; flex-direction:column; background:var(--bg); }}
  .chanhead {{ padding:9px 12px 8px; border-bottom:1px solid var(--bsub); font-size:12.5px;
    display:flex; gap:9px; align-items:baseline; }}
  .chanhead .sub {{ color:var(--tm); font-size:10px; }}
  .msgs {{ flex:1; padding:11px 12px; display:flex; flex-direction:column; gap:11px; }}
  .msg {{ display:grid; grid-template-columns:26px 1fr; gap:9px; }}
  .av {{ width:26px;height:26px;border-radius:7px;display:grid;place-items:center;
    font-size:10px;font-weight:700; }}
  .meta {{ display:flex; gap:8px; align-items:baseline; }}
  .meta b {{ font-size:12px; color:var(--tp); }}
  .time {{ font-size:9.5px; color:var(--tm); }}
  .txt {{ font-size:12px; color:var(--ts); line-height:1.5; margin-top:1px; }}
  .lnk {{ color:var(--link); }}
  .men {{ background:var(--men); color:var(--menfg); border-radius:4px; padding:0 3px; font-weight:700; }}
  code {{ background:var(--elev); border:1px solid var(--bsub); border-radius:4px; padding:0 3px;
    font-family:"Hack",monospace; font-size:10.5px; color:var(--tp); }}
  .warn {{ color:var(--warn); font-weight:600; }}
  .reacs {{ display:flex; gap:6px; margin-top:5px; }}
  .reac {{ background:var(--reac); border:1px solid var(--bstr); border-radius:9px;
    padding:1px 7px; font-size:10px; color:var(--ts); }}
  .divider {{ display:flex; align-items:center; gap:8px; color:var(--dang); font-size:9.5px; font-weight:700; }}
  .divider::before,.divider::after {{ content:""; flex:1; border-top:1px solid var(--dang); opacity:.55; }}
  .thread {{ margin-top:5px; font-size:10.5px; color:var(--acc); font-weight:600; }}
  .thread .time {{ color:var(--tm); font-weight:400; }}
  .composer {{ border-top:1px solid var(--bsub); margin:0 12px 11px; background:var(--elev);
    border:1px solid var(--bstr); border-radius:9px; padding:7px 9px; display:flex;
    align-items:center; gap:8px; }}
  .ph {{ color:var(--tm); font-size:11.5px; flex:1; }}
  .cbtn {{ width:20px;height:20px;border-radius:6px;display:grid;place-items:center;
    background:var(--hov); color:var(--ts); font-size:11px; }}
  .cbtn.acc {{ background:var(--acc); color:var(--accfg); font-weight:800; }}
  .swatches {{ display:grid; grid-template-columns:1fr 1fr; gap:3px 12px; margin:13px 0 11px; }}
  .sw {{ display:grid; grid-template-columns:15px 1fr auto; gap:6px; align-items:center;
    font-size:9.5px; }}
  .chip {{ width:15px;height:15px;border-radius:4px;border:1px solid rgba(255,255,255,.12); }}
  .swk {{ color:var(--tm); overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }}
  .swv {{ color:var(--ts); font-family:"Hack",monospace; font-size:9px; }}
  .ctrbox {{ border-top:1px solid #1a2130; padding-top:9px; }}
  .cttitle {{ font-size:9.5px; text-transform:uppercase; letter-spacing:.08em; color:#8695ab; margin-bottom:5px; }}
  .ctrow {{ display:flex; justify-content:space-between; align-items:center; font-size:10px; padding:1.5px 0; }}
  .ctl {{ color:#8695ab; }}
  .ctr {{ font-family:"Hack",monospace; font-size:9px; padding:1px 6px; border-radius:6px; }}
  .ctr.ok {{ background:rgba(63,185,80,.16); color:#5fd06f; }}
  .ctr.warn {{ background:rgba(217,164,65,.18); color:#e0b45c; }}
  .ctr.bad {{ background:rgba(229,83,75,.18); color:#f08a82; }}
</style></head>
<body><div class="page">
  <h1>slaq — identità dark, 3 candidate</h1>
  <p class="lead">Ogni colonna è una possibile identità: stessa struttura, palette diversa. Sotto trovi i
  ruoli semantici (non i singoli colori) e il contrasto WCAG misurato. Scegli una direzione e la
  formalizzo in token + la applico alla GUI reale.</p>
  <div class="grid">{cands}</div>
</div></body></html>"""


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=str(pathlib.Path(__file__).with_suffix(".html")))
    args = ap.parse_args()
    out = pathlib.Path(args.out)
    out.write_text(render_page(), encoding="utf-8")
    print(out)
    for c in CANDIDATES:
        worst = min(
            (contrast(c["roles"][fg], c["roles"][bg]) / need, label)
            for label, fg, bg, need in CONTRAST_CHECKS
        )
        print(f"  {c['key']} {c['name']}: worst = {worst[1]} ({worst[0]:.2f}x of target)")


if __name__ == "__main__":
    main()
